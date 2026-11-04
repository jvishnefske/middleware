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
  `transport::reqrep::tests::handler_error_codes_round_trip` (see K2).
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
  `zenoh::wire::tests::publication_attachments_round_trip_and_v1_still_decodes`;
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
  `g7_payload_at_limit_round_trips_through_subscription` (4 MiB − 140).
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
- [x] B11 A `BleServer` handler sees the connected peer's address as
  `RequestMeta::sender` when the binding reports it (`GattServer::peer`,
  default `None`, so existing bindings are unchanged); never a principal
  or a deadline —
  `ble::gatt::tests::b11_handler_sees_the_peer_address_as_sender`.

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
- [x] A15 `AdvertisingLink` joins a publisher and a subscriber into one
  `Publisher` + `Subscriber` and implements `Split` (K11). Samples report
  no gap (`SampleMeta::gap` is `None`): one sequence counts every key of a
  sender, so a jump seen through the scanner's keys is not a loss on them —
  `ble::advert::tests::k11_advertising_link_splits_into_publisher_and_subscriber`.

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
- [x] N15 Samples report no gap (`SampleMeta::gap` is `None`) even across
  a sequence jump: the sequence is per sender over all its keys (and the
  ESP base firmware numbers route chunks separately), so a receiver
  filtering some keys cannot tell a loss from the sender's other keys —
  `espnow::tests::n15_sequence_jumps_are_not_reported_as_gaps`.
- [x] N16 Over `RadioHalves` (an `EspNowSend` and an `EspNowReceive`,
  itself an `EspNowRadio`) the transport implements `Split`: `EspNowTx`
  publishes with the transport's sequence rule while `EspNowRx` receives
  with its filter slots, own address, dedup table and kept frame —
  `espnow::tests::k11_split_publishes_while_a_receive_waits`,
  `k11_split_rx_keeps_a_sample_that_did_not_fit`.

### Zenoh (`zenoh`)

- [x] Z1 Publish/subscribe round trip through a Zenoh session —
  `tests/zenoh.rs`: `z1_publish_subscribe_round_trip`,
  `z1_subscriber_buffer_too_small_is_error`.
- [x] Z2 Request is a Zenoh `get`; responder is a queryable running the pure
  handler — `z2_request_runs_handler_in_responder`,
  `z2_response_buffer_too_small_is_error`,
  `z2_request_without_responder_is_no_reply`,
  `zenoh::responder::tests::run_handler_splits_scratch_and_reports_errors`.
- [x] Z3 Handler errors are returned as Zenoh reply errors and surface to the
  requester — `z3_handler_error_surfaces_at_requester`,
  `z3_oversized_request_is_buffer_too_small_reply`,
  `zenoh::wire::tests::reply_error_payload_decodes_code_byte_and_sender`.
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
- [x] Z7 Publications carry an attachment (`mw`, kind, sender id hashed
  from the session's Zenoh id, sequence; since 0.5 also a stream id, 16
  bytes, see Z14; the 0.4 12-byte form still decodes) surfaced as
  `SampleMeta`; subscribers use `allowed_origin(Remote)`, so a session never
  receives its own publications (`with_local_delivery(true)` opts back in) —
  `z7_no_self_delivery_and_sender_meta_present`,
  `zenoh::wire::tests::publication_attachments_round_trip_and_v1_still_decodes`.
- [x] Z8 Cancelling `recv` loses no sample —
  `z8_cancelled_recv_loses_no_sample`.

The 0.5 additions (Z9–Z18). Unit tests are in `zenoh::{wire, subscriber,
responder, discovery}::tests`; the rest are in `tests/zenoh.rs`, which also
runs every conformance case over sessions joined on one loopback endpoint
with scouting off. All run under `--features zenoh`.

- [x] Z9 `request_with` honours `RequestOptions::timeout` as the Zenoh query
  timeout, falling back to the transport's default (`with_timeout`, 5 s);
  Zenoh's own `Timeout` error reply (from the querying session or the peer
  that routed the query) is not taken for a responder's error, so the call
  ends with `NoReply` —
  `z9_request_with_timeout_overrides_the_default`, `k_timeout_is_honoured`,
  `zenoh::wire::tests::reply_error_payload_decodes_code_byte_and_sender`.
- [x] Z10 A responder also declares the per-node queryable
  `@mw/node/<zid16>/<key>` feeding the same FIFO; `Target::Node(id)`
  queries it, so only that node answers; a node without a responder fails
  with `NoReply` at once (no queryable matches) —
  `z10_target_node_reaches_only_that_node`,
  `zenoh::wire::tests::node_and_service_keys`.
- [x] Z11 `Gather` is a `get` with `QueryTarget::All` and no consolidation:
  every responder's reply reaches the callback with the replier's
  `SenderId` (reply attachment, or the 8 bytes after the code in a reply
  error) and its result, bounded by `max_replies`, `Flow::Stop`, Zenoh
  finalising the query and the timeout —
  `k_gather_collects_every_responder`,
  `z11_gather_names_repliers_and_carries_their_errors`,
  `zenoh::wire::tests::query_and_reply_attachments_round_trip`.
- [x] Z12 Queries carry an attachment with the requester's sender id and
  its timeout; the handler sees them as `RequestMeta::sender` and
  `deadline_ms` (minus the time queued at the responder); `principal` is
  `None` (Zenoh authenticates no caller name); handler errors, including
  `Unavailable`, `DeadlineExceeded` and `Denied`, round trip —
  `k_handler_sees_caller_identity`, `k_handler_errors_round_trip`,
  `k_round_trip`,
  `zenoh::responder::tests::request_meta_carries_sender_and_remaining_deadline`.
- [x] Z13 `ZenohSubscriber::DELIVERY` is `Block` with per-sender order:
  Zenoh's FIFO handler blocks when full, the link stops being read and
  `publish` uses congestion control Block, so nothing is dropped and a
  publisher's samples arrive in order (caveats in `zenoh::subscriber`:
  head-of-line blocking, Zenoh closes a link blocked past
  `wait_before_close`); the FIFO depth is `with_queue_depth` (default 256) —
  `k_overload_matches_declared_delivery`,
  `zenoh::subscriber::tests::delivery_is_declared_block_in_order`.
- [x] Z14 Sequences are per key per transport value (a clone starts new
  streams) and the attachment names the stream, so `SampleMeta::gap` is
  exact for a subscriber to any subset of keys (`SeenTable::check_stream`;
  `None` for a stream's first sample) —
  `k_gap_is_reported`, `z14_gaps_are_per_key`,
  `zenoh::subscriber::tests::gaps_are_exact_per_sender_and_stream`,
  `zenoh::subscriber::tests::foreign_and_v1_attachments_report_no_gap`,
  `zenoh::wire::tests::stream_ids_separate_keys_and_origins`.
- [x] Z15 `ZenohDiscovery` announces a service as a record queryable plus a
  liveliness token on `@mw/svc/<kind>/<instance>` (record in the beacon
  encoding; node defaults to the announcing session) and browses with a
  liveliness subscriber with history: token appears → record fetched →
  `Up`; token gone (withdraw, session closed) → `Down` at once —
  `l_announce_then_withdraw`, `l_goodbye_removes_record_at_once`,
  `l_directory_expiry`,
  `z15_browse_sees_services_announced_before_it_with_the_announcer_as_node`,
  `z15_closing_the_announcer_session_is_down_and_try_next_does_not_wait`,
  `zenoh::discovery::tests::records_and_goodbyes_decode_from_their_keys`.
- [x] Z16 The Zenoh backend passes every conformance case of
  `tests/conformance.rs` (`ReqRepFixture`, `GatherFixture`,
  `PubSubFixture`, `DiscoveryFixture`) over loopback sessions —
  `tests/zenoh.rs`: `k_round_trip`, `k_timeout_is_honoured`,
  `k_handler_errors_round_trip`, `k_handler_sees_caller_identity`,
  `k_gather_collects_every_responder`,
  `k_overload_matches_declared_delivery`, `k_gap_is_reported`,
  `l_announce_then_withdraw`, `l_goodbye_removes_record_at_once`,
  `l_directory_expiry`.
- [x] Z17 Trust model: Zenoh session ids, attachments, tokens and
  queryables are self-asserted, so `Reply::sender`, `RequestMeta::sender`
  and `SampleMeta::sender` are the peer's claim. A `Target::Node(id)`
  request (and gather) accepts only replies whose replier id is `id`, so a
  peer squatting on `@mw/node/<zid16>/<key>` without claiming the id is
  ignored; one that also claims it is not detected. Authenticated identity
  needs Zenoh access control and authentication (router ACLs on
  `@mw/node/**`, `@mw/svc/**` and application keys), which the middleware
  does not configure —
  `z17_target_node_drops_replies_not_naming_the_node`,
  `zenoh::wire::tests::node_targets_accept_only_replies_naming_the_node`.
- [x] Z18 A browser's record fetch collects every reply until the query
  ends and keeps only successful replies on exactly the token's key whose
  record names the key's kind and instance; if two such records differ the
  service is refused (`ConflictingRecords`, no `Up`) instead of taking the
  first. The stable Zenoh API cannot tell which session owns a token, so a
  forger answering alone, or with identical bytes, is not detected (same
  trust model as Z17) —
  `z18_conflicting_service_records_are_refused`,
  `z18_records_for_another_service_are_ignored`,
  `zenoh::discovery::tests::differing_records_conflict_and_equal_ones_agree`,
  `zenoh::discovery::tests::records_must_name_the_token_kind_and_instance`.

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
  named subscribers get `[key_len][key][sender_len][sender][payload]`
  (and 0.5 subscribers the gap too, G12), so `MAX_PAYLOAD` is 4 MiB − 140 —
  `grpc::bus::tests::g8_bus_refuses_self_delivery_and_fills_the_sender`
  (legacy stream), `grpc::bus::tests::g8_items_with_sender_round_trip_and_reject_garbage`,
  `g7_limits_fit_tonic_default_message_size`.
- [x] G10 The responder hands the handler the caller's `RequestMeta`:
  `sender` from `mw-client` (every `GrpcTransport` request now sends it),
  `deadline_ms` from `grpc-timeout` turned into a deadline on arrival and
  measured when `serve_one` runs the handler (rounded down, 0 once passed),
  and `principal` only from a configured `PrincipalSource` (default none:
  an `x-forwarded-client-cert` header is ignored) —
  `grpc::responder::tests::g10_call_meta_reads_client_timeout_and_only_trusted_principals`,
  `grpc::responder::tests::g10_remaining_ms_rounds_down_and_saturates`,
  `tests/grpc.rs`: `g10_handler_sees_client_id_and_deadline_but_no_untrusted_principal`.
- [x] G11 `request_with` writes `options.timeout` as `grpc-timeout`
  (`grpc::timeout::GrpcTimeout`, shared with `istio`) and bounds the call
  locally; expiry, and a server's own `CANCELLED` arriving after the
  deadline, are `DEADLINE_EXCEEDED`; a full responder queue and a refused
  connection are `UNAVAILABLE`; both map to the same `HandlerError` through a
  real in-process server —
  `g11_busy_responder_and_refused_connection_are_unavailable`,
  `k15_grpc_timeout_is_honoured`, `k15_grpc_handler_errors_round_trip`.
- [x] G12 A `GrpcSubscriber` queues `queue_depth` samples locally
  (`with_queue_depth`, a power of two, default 64) filled by a task reading
  the stream, drops the oldest when the application falls behind
  (`DELIVERY = DropOldest`, ordered), keeps a sample that did not fit the
  buffer for a retry, and reports in `SampleMeta::gap` the samples lost
  before each one, locally or to the bus's broadcast lag (`mw-gap: report`
  items carry a big-endian `u16` gap) —
  `g12_subscriber_queue_drops_oldest_and_reports_the_gap`,
  `grpc::bus::tests::g12_items_with_gap_round_trip_and_reject_garbage`,
  `grpc::bus::tests::g12_broadcast_lag_is_carried_by_the_next_item`,
  `k15_grpc_overload_matches_declared_delivery`, `k15_grpc_gap_is_reported`.
- [x] G13 `grpc.health.v1.Health` `Check` and `Watch`, protobuf
  hand-encoded on `RawCodec` (unknown fields skipped, malformed messages
  refused), served by `HealthService` (mounted with
  `GrpcServer::with_health`) and called with `HealthClient`; `Check` of an
  unknown name is `NOT_FOUND`, `Watch` sends the current status and then
  each change and ends with the announcer; `GrpcAnnouncer` implements
  `discovery::Announce` (announce → the kind `SERVING`, withdraw of its last
  instance → `NOT_SERVING`) —
  `grpc::health::tests::g13_health_messages_round_trip_and_skip_unknown_fields`,
  `grpc::health::tests::g13_malformed_health_messages_are_refused`,
  `grpc::health::tests::g13_health_table_is_a_bounded_value`,
  `grpc::health::tests::g13_announcer_serves_while_any_instance_is_announced`,
  `grpc::health::tests::g13_watch_stream_sends_current_then_changes_only`,
  `grpc::server::tests::bus_paths_are_recognised`,
  `tests/grpc.rs`: `g13_health_check_and_watch_over_loopback`.
- [x] G14 gRPC passes every conformance case of the traits it implements
  (`Requester`, `Gather` through `MeshClient`, `Publisher`/`Subscriber`,
  `Announce`/`Browse` through `GrpcAnnouncer` and `MeshBrowse`) over
  in-process servers on 127.0.0.1:0, the handler seeing sender, SPIFFE
  principal and deadline (the requester injects an Envoy-style XFCC
  header in place of the sidecar) — `tests/grpc.rs`
  `conformance_cases::k15_grpc_*` (ten cases) *(istio)*.

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
- [x] I6 `istio::peer_principal` takes the caller's SPIFFE ID from Envoy's
  `x-forwarded-client-cert`: the first valid `URI=spiffe://…` of the last
  (closest) element, honouring quoting (`,` `;` `=` inside quotes, `\"`
  escapes) and refusing unbalanced quotes and non-SPIFFE URIs; the
  responder uses it only with `PrincipalSource::SidecarXfcc`, documented as
  sound only behind the sidecar that sets or sanitizes the header —
  `grpc::istio::tests::i6_peer_principal_takes_the_spiffe_uri_of_the_last_element`,
  `grpc::istio::tests::i6_quoted_separators_do_not_split_elements_or_pairs`,
  `grpc::istio::tests::i6_malformed_or_missing_principals_are_none`,
  `k15_grpc_handler_sees_sender_principal_and_deadline`.
- [x] I7 `mesh::MeshClient` routes requests to the endpoints of its kind in
  a `Directory` (each endpoint's `:authority` through the sidecar or
  directly, `MeshDialer`): `Target::Any` in turn, `Target::Node` to the
  endpoint naming that node, `UNAVAILABLE` at once when none does;
  `Gather` calls every endpoint concurrently, bounded by `max_replies`,
  `Flow::Stop` and the timeout (default 5 s), each `Reply.sender` the
  endpoint's node —
  `grpc::mesh::tests::i7_endpoint_authorities`,
  `grpc::mesh::tests::i7_targets_select_by_kind_and_node`,
  `grpc::mesh::tests::i7_node_target_without_endpoint_is_unavailable_at_once`,
  `tests/grpc.rs`: `mesh::i7_mesh_client_routes_by_node_and_gathers_every_endpoint`,
  `k15_grpc_gather_collects_every_responder`.
- [x] I8 `mesh::MeshBrowse<B: Browse>` reports an endpoint of `B` up only
  while its health `Watch` says `SERVING`, and down on any other status,
  when the watch ends or fails, and when `B` reports it down; at most `N`
  endpoints are tracked and an event leaves the table only once copied
  into the caller's buffer —
  `grpc::mesh::tests::i8_table_reports_up_only_while_serving_and_bounds_entries`,
  `grpc::mesh::tests::i8_status_errors_end_the_watch`,
  `tests/grpc.rs`: `mesh::i8_mesh_browse_reports_up_only_while_serving`,
  `mesh::i8_mesh_browse_reports_down_when_dns_withdraws`,
  `k15_grpc_announce_then_withdraw`, `k15_grpc_goodbye_removes_record_at_once`.
### mDNS service discovery (`mdns`)

`MdnsSocket` browses DNS-SD service types (`_service._tcp|_udp.domain`)
over mDNS; since 0.5 it does so as a `discovery::Browse` (L12), and the
`ServiceDiscovery` trait it had before the 0.5 release (never in a
released version) is gone. The message format is pure, always
built and lives in `dns::wire` since 0.5 (`mdns::wire` re-exports it); the
tokio socket is behind the `mdns` feature and queries as a one-shot querier
from an ephemeral port, so it coexists with a system mDNS daemon on port
5353. The D tests below kept their names when the codec moved. Since 0.5
`MdnsAnnouncer` (`mdns::announce`) is the responder side, a
`discovery::Announce`: the pure `announce::table::AnnounceTable` decides
probes, conflict renames, announcements, answers and goodbyes, and a thin
tokio shell sends them. Both sockets apply the origin policy of
`mdns::origin` (D11).

- [x] D1 Service types are validated (`_`-prefixed first label, `_tcp` or
  `_udp` second, ≥ 3 labels, labels ≤ 63 bytes, name ≤ 255 bytes) —
  `dns::wire::tests::service_types_are_validated`.
- [x] D2 A browse is one PTR question, class IN, with the caller's id, and
  is refused rather than truncated in a short buffer —
  `dns::wire::tests::query_encodes_one_ptr_question`.
- [x] D3 A response's PTR, SRV, TXT and A records, with compressed names, are
  joined into a resolved instance (label, host, port, address, TXT values,
  ASCII case ignored in names and TXT keys) —
  `dns::wire::tests::response_resolves_an_instance_from_compressed_records`,
  `dns::wire::tests::other_services_and_queries_yield_no_instances`.
- [x] D4 A PTR with TTL 0 is a goodbye; a PTR without its SRV is unresolved —
  `dns::wire::tests::goodbye_and_ptr_only_responses`.
- [x] D5 Malformed messages are errors, never panics or hangs: every
  truncation, pointer loops, reserved label types, bad RDATA lengths and
  names over 255 bytes — `dns::wire::tests::malformed_messages_are_errors`,
  `dns::wire::tests::write_text_refuses_to_truncate`.
- [x] D6 `MdnsSocket` sends the query to its destination with a fresh id per
  browse, skips queries and returns responses —
  `mdns::socket::tests::browse_sends_a_ptr_query_and_recv_resolves_the_answer`,
  `mdns::socket::tests::queries_are_skipped_and_ids_advance`.
- [x] D7 A response that does not fit the caller's buffer is an error and is
  kept for one more call (`MAX_SHORT_BUFFER_RETRIES`); if that call's
  buffer is too small too, the error is reported again and the response is
  dropped, so an oversized datagram never blocks the socket;
  `discard_kept` drops it at once; `try_recv` is `None` when nothing has
  arrived; a malformed datagram is an error and is dropped —
  `mdns::socket::tests::try_recv_is_none_when_empty_and_keeps_an_oversized_response`,
  `mdns::socket::tests::an_oversized_response_is_dropped_after_one_retry`,
  `mdns::socket::tests::malformed_datagrams_are_errors_then_dropped`.
- [x] D8 Every receive call is bounded in datagrams (`MAX_IGNORED_DATAGRAMS`)
  and a cancelled `recv` loses no response —
  `mdns::socket::tests::ignored_limit_bounds_try_recv`,
  `mdns::socket::tests::cancelled_recv_loses_no_response`.
- [x] D9 The pure core builds for `thumbv6m-none-eabi`; `mdns` enables `std`
  and no embedded feature — `scripts/check-features.sh`.
- [x] D10 Tests put no multicast traffic on the network: the socket is
  aimed at a loopback responder, and the announcer at a loopback browser or
  rival with an explicit destination (which also skips the group join) —
  `mdns::socket::tests::*`, `mdns::announce::tests::*`, `tests/mdns.rs`.
- [x] D11 Origin validation (RFC 6762 §11): unless a destination was set
  explicitly, a datagram is accepted only from an on-link source (IPv4
  loopback, link-local, or a connected subnet read from `/proc/net/route`;
  IPv6 loopback or link-local), a response only from port 5353, and a
  one-shot querier accepts only responses carrying one of its last 8 query
  ids; refused datagrams are skipped like queries and the source of the
  last response is kept (`last_source`). The IP TTL 255 check is not done:
  it needs `unsafe` (socket2 has no `IP_RECVTTL` setter, and its `recvmsg`
  hands ancillary data back as `MaybeUninit` bytes) —
  `mdns::origin::tests::proc_net_route_yields_connected_subnets_only`,
  `mdns::origin::tests::on_link_sources_are_loopback_link_local_or_connected`,
  `mdns::origin::tests::responses_need_port_5353_and_an_outstanding_id_when_on_link`,
  `mdns::origin::tests::outstanding_keeps_the_last_ids`,
  `mdns::socket::tests::on_link_policy_refuses_wrong_port_and_unknown_ids`.
- [x] D12 No datagram wedges `Browse::next`: one that does not parse is
  skipped, an instance whose event cannot be built (label not UTF-8, host
  name too long) or needs more than `MAX_EVENT_LEN` (1024) bytes is
  skipped, and an event too large for the caller's buffer is kept for one
  retry and then skipped —
  `mdns::socket::tests::poison_datagrams_do_not_wedge_browse`.
- [x] D13 The announcer probes an instance name three times 250 ms apart
  (query type ANY, QU bit, proposed SRV and TXT in the authority section),
  then announces it twice 1 s apart (PTR, SRV, TXT, A/AAAA in the answer
  section, cache-flush on the unique records); `announce` returns after the
  first announcement, `step` sends the second; re-registering an announced
  instance re-announces without probing —
  `mdns::announce::table::tests::probes_three_times_then_announces_twice`,
  `mdns::announce::table::tests::re_registering_an_announced_instance_announces_without_probing`,
  `mdns::announce::tests::announces_after_probing_and_answers_a_browse`.
- [x] D14 A response carrying a record of the probed name with other data
  is a conflict: the instance is renamed `name (2)` … `name (9)` and probed
  again, and after 8 renames `announce` fails with
  `AnnounceError::Conflict`; our own records looped back are not
  conflicts —
  `mdns::announce::table::tests::a_conflict_while_probing_renames_and_probes_again`,
  `mdns::announce::table::tests::renames_fit_a_label`,
  `mdns::announce::tests::a_rival_answer_while_probing_renames_the_instance`.
- [x] D15 Announced instances answer PTR, SRV, TXT, A/AAAA (and ANY)
  queries: multicast by default, unicast to the source for QU questions,
  legacy unicast (id and question echoed, TTL ≤ 10 s, no cache-flush) for
  queries from a port other than 5353; PTRs the query lists as known
  answers with at least half the TTL are suppressed; probing instances and
  malformed queries get no answer —
  `mdns::announce::table::tests::announced_instances_answer_queries`,
  `mdns::announce::table::tests::known_answers_are_suppressed`.
- [x] D16 Withdraw sends a goodbye (PTR, SRV, TXT with TTL 0) for an
  announced instance and nothing for one still probing or unknown;
  registration refuses locators, `Local`, unspecified addresses, unmappable
  kinds and invalid labels, and holds at most `MAX_INSTANCES` (8) —
  `mdns::announce::table::tests::withdraw_sends_a_goodbye_only_once_announced`,
  `mdns::announce::table::tests::registration_is_validated_and_bounded`.
- [x] D17 On-link subnets come only from broadcast LAN interfaces: a
  `/proc/net/route` route counts only if its interface is up, broadcast-
  and multicast-capable and not point-to-point (`/sys/class/net/<if>/flags`;
  VPN, `tun` and WireGuard routes never count, nor routes of interfaces
  whose flags cannot be read) and its prefix is at least /8
  (`MIN_PREFIX_LEN`) —
  `mdns::origin::tests::proc_net_route_yields_connected_subnets_only`,
  `mdns::origin::tests::interface_flags_select_broadcast_lans`.
- [x] D18 A received datagram cannot stop the announcer: a reply to it that
  cannot be encoded or sent is dropped and counted (`dropped_replies`), and
  a reply is never unicast to port 0 or to an unspecified, multicast or
  broadcast source (limited or a known subnet's) —
  `mdns::origin::tests::replies_are_never_unicast_to_broadcast_multicast_or_port_0`,
  `mdns::announce::tests::replies_that_cannot_go_out_are_dropped_not_fatal`.

### Transport contract 0.5 (K…)

The 0.5 core contract in `transport` (split into `transport::meta`,
`transport::pubsub` and `transport::reqrep`, re-exported from
`transport`): handlers take a `Request` with `RequestMeta`, requests take
`RequestOptions` (timeout, target), scatter/gather is `Gather`, every
subscriber declares its `Delivery`, samples report gaps, and `Split`
divides a link into halves. `tests/conformance.rs` holds generic cases
every backend runs; `tests/memory.rs` runs them against the in-process
backend `transport::memory` (feature `memory`, enabled for tests by the
crate's self dev-dependency). Tests marked *(feature)* run under
`cargo test --all-features` (or that feature), not the default gate.

- [x] K1 A `Handler` receives a `Request { key, body, meta }` and writes
  the response into the caller's buffer; closures of that shape are
  handlers (`handler_fn` fixes their argument types) —
  `transport::reqrep::tests::closures_are_handlers_and_see_meta`,
  `tests/memory.rs`: `k_round_trip`.
- [x] K2 `HandlerError` is non-exhaustive and adds `Unavailable` (code 6)
  and `DeadlineExceeded` (code 7); unknown codes map to `Internal`; gRPC maps
  them to and from `UNAVAILABLE` / `DEADLINE_EXCEEDED`, Zenoh carries the
  code byte —
  `transport::reqrep::tests::handler_error_codes_round_trip`,
  `grpc::status::tests::handler_errors_round_trip_through_codes` and
  `grpc::status::tests::codes_map_back_to_handler_errors` *(grpc)*,
  `zenoh::wire::tests::reply_error_payload_decodes_code_byte_and_sender` *(zenoh)*.
- [x] K3 `RequestOptions` (timeout, `Target::{Any, Node}`) builds with
  const methods and converts its timeout to a saturating millisecond
  deadline; `Requester::request` is `request_with` with
  `RequestOptions::DEFAULT` —
  `transport::reqrep::tests::request_options_build_and_convert_timeouts`,
  `transport::reqrep::tests::request_is_request_with_defaults`,
  `typed::tests::typed_request_with_passes_options`.
- [x] K4 `request_with` honours its timeout: with nobody serving it
  returns the backend's timeout error no earlier than the timeout and
  within the backend's slack —
  `tests/conformance/reqrep.rs`: `timeout_is_honoured`, run by
  `tests/memory.rs`: `k_timeout_is_honoured`;
  `transport::memory::tests::requests_reach_a_responder_with_meta_and_honour_timeouts`.
  Backends without a clock (serial link, BLE GATT client) document that
  the caller bounds the call; Zenoh passes it as the query timeout and
  gRPC as `grpc-timeout` plus a local deadline.
- [x] K5 `Unavailable`, `DeadlineExceeded` and `Denied` returned by a
  remote handler reach the requester as the same `HandlerError` —
  `tests/conformance/reqrep.rs`: `handler_errors_round_trip`, run by
  `tests/memory.rs`: `k_handler_errors_round_trip`.
- [x] K6 The handler sees the caller's `RequestMeta`: sender, the
  authenticated principal and the remaining deadline, as far as the backend
  knows them (the fixture declares what it provides) —
  `tests/conformance/reqrep.rs`: `handler_sees_caller_identity`, run by
  `tests/memory.rs`: `k_handler_sees_caller_identity`.
- [x] K7 `Gather::request_each` sends once and passes every reply to the
  callback, bounded by `max_replies`, `Flow::Stop` and time, and returns
  the number seen —
  `tests/conformance/reqrep.rs`: `gather_collects_every_responder`, run
  by `tests/memory.rs`: `k_gather_collects_every_responder`.
- [x] K8 Every `Subscriber` declares `DELIVERY` (`Overflow::{DropNewest,
  DropOldest, Block}`, per-sender order) and `LIMITS` as required
  constants, so a backend that omits them does not compile; every in-tree
  backend declares them (Zenoh: `Block`; serial since its inbox (S8),
  ESP-NOW, BLE GATT, BLE advertising: `DropNewest`; gRPC, memory:
  `DropOldest`); the
  declared behaviour holds under overload —
  `tests/conformance/pubsub.rs`: `overload_matches_declared_delivery`, run
  by `tests/memory.rs`: `k_overload_matches_declared_delivery`.
- [x] K9 `SampleMeta::gap` reports samples known lost before this one;
  `dedup::SeenTable::check` / `SeqCheck` compute it from 8-bit wrapping
  sequences per (sender, stream) (first, next with loss, repeat, behind) —
  `dedup::tests::gaps_are_computed_from_wrapping_sequences`,
  `tests/conformance/pubsub.rs`: `gap_is_reported`, run by
  `tests/memory.rs`: `k_gap_is_reported`.
- [x] K10 `Target::Node` reaches only that node's responder; with no
  responder on it the request fails at once —
  `transport::memory::tests::requests_reach_a_responder_with_meta_and_honour_timeouts`.
- [x] K11 `Split` divides a link into a `Publisher` half and a
  `Subscriber` half usable concurrently, each borrowing disjoint parts of
  the link (no shared ownership, no `RefCell`): the serial link over a
  `Duplex` (`SerialTx`, `SerialRx`), ESP-NOW over `RadioHalves`
  (`EspNowTx`, `EspNowRx`) and BLE advertising as an `AdvertisingLink`
  (`AdvertTx`, `AdvertRx`) —
  `serial::link::tests::k11_split_halves_publish_while_a_receive_waits`,
  `k11_split_rx_drains_the_inbox_first`, `tests/serial.rs`:
  `k11_serial_split_publishes_while_subscribed` *(serial)*;
  `espnow::tests::k11_split_publishes_while_a_receive_waits`,
  `k11_split_rx_keeps_a_sample_that_did_not_fit` *(espnow)*;
  `ble::advert::tests::k11_advertising_link_splits_into_publisher_and_subscriber`
  *(ble)*.
- [x] K12 A backend plugs into the conformance harness by implementing
  `ReqRepFixture`, `GatherFixture`, `PubSubFixture` and/or
  `DiscoveryFixture` and calling the generic cases (documented in
  `tests/conformance.rs`) — `tests/memory.rs` (all ten cases).
- [x] K13 The 0.5 contract (`transport::{meta, pubsub, reqrep}`) is
  `no_std`, heap free and always built; the memory backend needs
  `memory` (std) — `scripts/check-features.sh` (C1 and F2 sets).
- [x] K14 `typed::TypedHandler` adapts `Fn(&Req, &RequestMeta) ->
  Result<Resp, HandlerError>` into a `Handler` (undecodable request →
  `InvalidRequest`, oversized response → `BufferTooSmall`) —
  `typed::tests::typed_handler_decodes_calls_and_encodes`.
- [x] K15 Every backend passes every conformance case for the traits it
  implements, with caller identity carried where the transport
  authenticates it (Istio SPIFFE principal on gRPC):
  - memory: `tests/memory.rs`, all ten cases (`k_round_trip`,
    `k_timeout_is_honoured`, `k_handler_errors_round_trip`,
    `k_handler_sees_caller_identity`, `k_gather_collects_every_responder`,
    `k_overload_matches_declared_delivery`, `k_gap_is_reported`,
    `l_announce_then_withdraw`, `l_goodbye_removes_record_at_once`,
    `l_directory_expiry`);
  - Zenoh: `tests/zenoh.rs`, the same ten case names over loopback
    sessions *(zenoh)*;
  - gRPC: `tests/grpc.rs`, `conformance_cases::k15_grpc_round_trip`,
    `k15_grpc_timeout_is_honoured`, `k15_grpc_handler_errors_round_trip`,
    `k15_grpc_handler_sees_sender_principal_and_deadline` (SPIFFE ID from
    the sidecar's XFCC), `k15_grpc_gather_collects_every_responder`
    (`MeshClient`), `k15_grpc_overload_matches_declared_delivery`,
    `k15_grpc_gap_is_reported`, `k15_grpc_announce_then_withdraw` and
    `k15_grpc_goodbye_removes_record_at_once` (`GrpcAnnouncer` health
    behind `MeshBrowse`), `k15_grpc_directory_expiry` *(istio)*;
  - serial link (the in-memory fake link pair standing in for `mwctl`'s
    port): `tests/serial.rs`, `k15_serial_round_trip`,
    `k15_serial_timeout_is_honoured` (caller-side timer: the link has no
    clock), `k15_serial_handler_errors_round_trip`,
    `k15_serial_handler_sees_caller_identity`,
    `k15_serial_overload_matches_declared_delivery`,
    `k15_serial_gap_is_reported` (the link reports none),
    `k15_serial_announce_then_withdraw`,
    `k15_serial_goodbye_removes_record_at_once` (`PubSubDiscovery` over a
    split link), `k15_serial_directory_expiry`; no `Gather` (one peer)
    *(serial)*;
  - mDNS: `tests/mdns.rs`, `l_announce_then_withdraw`,
    `l_goodbye_removes_record_at_once`, `l_directory_expiry` (L14)
    *(mdns)*;
  - unicast DNS-SD: `tests/dns_sd.rs`, the same three (L16) *(dns-sd)*.

  ESP-NOW and BLE have no async fake in `tests/`; their contract points are
  covered by unit tests (K8, K11, N15, N16, A15, B11).
- [x] K16 One service kind is discovered and called across backends in one
  process on loopback: announced over Zenoh (two responder sessions) and
  mDNS (announcer aimed at a loopback browser, no multicast), listed by a
  stub cluster DNS server whose SRV glue points at gRPC servers serving
  health, browsed through `MeshBrowse<UnicastDnsSd>`; every event feeds one
  `Directory`; then Zenoh `request_with` with a timeout and `Target::Node`
  of the discovered node, a Zenoh gather of both responders, a gRPC
  `MeshClient` request and gather over the directory (three replies), and
  `Target::Node` on the mesh client failing `Unavailable` (no DNS-SD
  record names a node); withdrawals over each path empty their records.
  The example finishes in about a second and exits 0 —
  `examples/discover_and_call.rs`:
  `tests::discover_and_call_finds_and_calls_every_responder` (run by
  `cargo test --all-features`; the example declares `test = true`).

### Service discovery (L…)

`discovery` (always built, `no_std`, no heap): `Service` records of a
`ServiceKind` (a middleware key) at an `Endpoint`, the `Announce` /
`Browse` traits, the pure fixed-capacity `Directory`, and
`beacon::PubSubDiscovery`, discovery over any pub/sub transport with a
versioned `disc/<kind>` beacon. `dns::wire` is the DNS codec, now with an
encoder; `MdnsSocket` implements `Browse`.

- [x] L1 A kind maps to a DNS-SD type by a fixed reversible rule (`/` →
  `-`, lowercase letters and digits kept, anything else refused, ≤ 15
  characters with a letter, RFC 6335): `cam/jpeg` → `_cam-jpeg._tcp`
  (`.local` with the domain), and back —
  `discovery::record::tests::kinds_map_to_dns_sd_types_and_back`,
  `discovery::record::tests::unmappable_kinds_are_refused`.
- [x] L2 An announcement with TTL 0 is a `Down` event; events copy into a
  caller buffer and fail instead of truncating —
  `discovery::record::tests::events_follow_ttl_and_copy_into_buffers`.
- [x] L3 `Directory::observe` is a pure value transition reporting
  `Added`, `Refreshed`, `Updated`, `Removed` or `Ignored`; a goodbye (a
  `Down`, or `Up` with TTL 0) removes the record at once —
  `discovery::directory::tests::up_refresh_update_and_goodbye`,
  `tests/conformance/discovery.rs`: `goodbye_removes_record_at_once`, run
  by `tests/memory.rs`: `l_goodbye_removes_record_at_once`.
- [x] L4 Records expire exactly at their TTL (`expire`, `expired`,
  `next_expiry`) and are due for refresh at 80 % of it (`next_refresh`) —
  `discovery::directory::tests::records_expire_on_their_ttl_and_refresh_at_80_percent`,
  `tests/conformance/discovery.rs`: `directory_expiry`, run by
  `tests/memory.rs`: `l_directory_expiry`.
- [x] L5 The directory holds at most `N` records with bounded fields; a
  new record when full is `Full`, an over-long field `Rejected`, and no
  existing record is evicted —
  `discovery::directory::tests::capacity_and_bounds_are_enforced`.
- [x] L6 Beacon format version 1 round-trips every endpoint form, ignores
  trailing bytes (later versions append), refuses other versions, unknown
  flags or tags, bad strings and invalid TXT, and is never truncated;
  beacon keys are `disc/<kind>` —
  `discovery::beacon::tests::beacons_round_trip_every_endpoint`,
  `discovery::beacon::tests::malformed_beacons_are_refused`,
  `discovery::beacon::tests::beacon_keys_carry_the_kind`.
- [x] L7 `PubSubDiscovery` announces (and keeps up to `A` services),
  browses the filtered kinds, withdraws with a goodbye, and re-announces
  every half TTL driven by the pure `due` and the `tick` shell call —
  `discovery::beacon::tests::pubsub_discovery_announces_browses_and_reannounces`,
  `tests/conformance/discovery.rs`: `announce_then_withdraw`, run by
  `tests/memory.rs`: `l_announce_then_withdraw`.
- [x] L8 `dns::wire::MessageWriter` encodes PTR, SRV, TXT, A and AAAA
  records with name compression, keeps section order, and fails instead
  of truncating; every encoded record decodes back, and the DNS-SD join
  resolves it —
  `dns::wire::tests::every_record_type_round_trips_with_compression`,
  `dns::wire::tests::writer_refuses_to_truncate_and_keeps_section_order`.
- [x] L9 Queries for any type with the recursion-desired bit (unicast
  resolvers) encode correctly; text names with escapes and TXT entries are
  validated —
  `dns::wire::tests::unicast_queries_set_rd_and_any_type`,
  `dns::wire::tests::text_names_are_validated_and_unescaped`,
  `dns::wire::tests::txt_writer_builds_valid_rdata`.
- [x] L10 Parsing is bounded against hostile messages: more than
  `MAX_RECORDS` (64) resource records is `TooManyRecords` before any record
  is parsed, so the PTR × records join of `Response::instances` is at most
  64 × 64; the writer refuses to build such a message —
  `dns::wire::tests::records_over_the_cap_are_refused_before_parsing`.
- [x] L11 `discovery` and `dns::wire` are always built and `no_std` on
  `thumbv6m-none-eabi` — `scripts/check-features.sh` (C1 build and the
  check that their module declarations carry no cfg gate).
- [x] L12 `MdnsSocket` implements `Browse`: an exact kind is browsed as its
  DNS-SD type in `.local`, a prefix is refused, resolved instances are `Up`
  (socket address, or SRV host when no address record came), goodbyes are
  `Down`, and an event too large for the buffer is kept for one retry
  (D12) —
  `mdns::socket::tests::browse_maps_the_kind_and_reports_up_then_down`,
  `mdns::socket::tests::browse_reports_an_unaddressed_instance_by_host`
  *(mdns)*.
- [x] L13 `typed::TypedBrowser` decodes a service's description from its
  TXT entry `m=<bytes>` with a codec (`typed::encode_txt` writes it) —
  `typed::tests::typed_browser_decodes_txt_descriptions`.
- [ ] L14 *(phase 2)* An mDNS announcer (`Announce` on `MdnsSocket`) and
  a unicast DNS-SD resolver (`dns::unicast`, feature `dns-sd`) pass the
  discovery conformance cases on loopback.
  - [x] mDNS half: `MdnsAnnouncer` (the `Announce` side, a type of its own
    rather than `MdnsSocket`) announcing to an `MdnsSocket` browser over
    loopback passes every discovery case — `tests/mdns.rs`
    (`l_announce_then_withdraw`, `l_goodbye_removes_record_at_once`,
    `l_directory_expiry`) *(mdns)*.
- [x] L15 `dns-sd` enables `std` and no embedded feature, and no embedded
  feature set enables it (or any host feature) —
  `scripts/check-features.sh`.
- [x] L16 *(the unicast half of L14)* `dns::unicast::UnicastDnsSd`
  (feature `dns-sd`) passes the discovery conformance cases on loopback,
  against a stub DNS server whose fixture table the announcer edits —
  `tests/dns_sd.rs`: `l_announce_then_withdraw`,
  `l_goodbye_removes_record_at_once`, `l_directory_expiry` *(dns-sd)*.
- [x] L17 `ResolvConf::parse` reads `nameserver` (at most 3, numeric IPv6
  scope), `search` / `domain` (last wins, at most 6), `options ndots`,
  `timeout`, `attempts` (clamped) and ignores comments and malformed lines;
  a pod's namespace and cluster domain come from its search list —
  `dns::unicast::resolv::tests::kubernetes_pod_configuration`,
  `dns::unicast::resolv::tests::variants_comments_limits_and_malformed_lines`
  *(dns-sd)*.
- [x] L18 `ServiceMap` maps up to `M` kinds to a Service, namespace and
  port name, validated as Kubernetes does (RFC 1123 labels, IANA port
  names), and builds `_<port>._tcp.<service>.<namespace>.svc.<domain>`
  (cluster domain configurable, default `cluster.local`, every name ≤ 253
  bytes) —
  `dns::unicast::map::tests::srv_names_follow_the_kubernetes_dns_schema`,
  `dns::unicast::map::tests::invalid_parts_are_refused` *(dns-sd)*.
- [x] L19 A reply is accepted only from the queried nameserver's address,
  with the query's id and exactly its question; anything else is skipped
  (a spoofed pod is never reported), and with only mismatched replies the
  query times out after its attempts. A reply with TC is
  `UnicastError::Truncated` (no TCP fallback); an error code is
  `UnicastError::Answer`; neither changes the endpoints already reported —
  `dns::unicast::answer::tests::only_the_matching_reply_is_accepted`,
  `tests/dns_sd.rs`: `mismatched_ids_and_spoofed_sources_are_ignored`,
  `truncated_and_failed_replies_are_errors_that_keep_endpoints`
  *(dns-sd)*.
- [x] L20 Queries carry RD, an EDNS(0) OPT record for 1232 bytes
  (`MessageWriter::edns`) and a fresh id per query and attempt; each
  attempt waits at most the timeout —
  `dns::wire::tests::edns_opt_is_written_and_decoded`, `tests/dns_sd.rs`:
  `targets_without_glue_are_resolved_with_a_then_aaaa`,
  `mismatched_ids_and_spoofed_sources_are_ignored` *(dns-sd)*.
- [x] L21 Each SRV owned by the query name is one endpoint named by its
  target's first label (one per pod of a headless Service); A/AAAA glue in
  the reply gives `Endpoint::Socket`, else the target is resolved with A,
  then AAAA, and without an address it is `Endpoint::Host`; the TTL is the
  smallest used; NXDOMAIN or no SRV is an empty set, not an error —
  `dns::unicast::answer::tests::headless_srv_answer_with_glue`,
  `dns::unicast::answer::tests::response_codes_and_empty_answers`,
  `dns::unicast::answer::tests::address_answers_take_the_first_of_each_family`,
  `tests/dns_sd.rs`: `targets_without_glue_are_resolved_with_a_then_aaaa`,
  `nxdomain_is_an_empty_service` *(dns-sd)*.
- [x] L22 The pure `Tracker` turns successive answers into `Down` events
  for vanished instances, then `Up` for new or changed ones; an unchanged
  instance is silent until 80 % of the TTL it was last reported with has
  passed, then reported again so a `Directory` fed with the events keeps
  it; TTL 0 is reported as 1 s; capacity and over-long records are counted,
  not reported —
  `dns::unicast::tracker::tests::answers_become_up_and_down_diffs`,
  `dns::unicast::tracker::tests::duplicates_ttl_zero_and_capacity`,
  `dns::unicast::tracker::tests::unchanged_instances_are_refreshed_before_a_directory_expires_them`,
  `tests/dns_sd.rs`: `headless_service_pods_come_and_go_across_ttl_expiries`
  (2 → 3 → 1 pods) *(dns-sd)*.
- [x] L23 Re-resolution is due at 80 % of the smallest resolved TTL (the
  `Directory::next_refresh` rule), after empty answers and failures at
  fixed intervals, all clamped to the `Schedule`'s bounds —
  `dns::unicast::tracker::tests::re_resolution_is_due_at_80_percent_of_the_ttl_clamped`,
  `tests/dns_sd.rs`: `headless_service_pods_come_and_go_across_ttl_expiries`
  *(dns-sd)*.
- [x] L24 `UnicastDnsSd::browse` takes an exact kind or a prefix and
  browses every mapped kind it selects (none is `Unmapped`); `next` before
  `browse` is `NotBrowsing`; the nameserver is the builder's or the
  resolver configuration's first —
  `tests/dns_sd.rs`: `browse_selects_mapped_kinds_by_prefix`,
  `nameserver_from_the_resolver_configuration` *(dns-sd)*.
- [x] L25 Transaction entropy against off-path cache poisoning (RFC 5452):
  every attempt of every query is sent from a new socket on a fresh
  ephemeral port, and its id comes from the operating system's CSPRNG
  (`dns::unicast::query_id`, `getrandom`), with no generator state kept in
  the process —
  `tests/dns_sd.rs`: `targets_without_glue_are_resolved_with_a_then_aaaa`
  (distinct source port and id per query), `query_ids_come_from_the_os_csprng`
  *(dns-sd)*.
- [x] L26 A flood of bogus datagrams cannot suppress or poison a
  resolution: each query socket is connected to the nameserver, so the
  kernel drops other sources, and skipped datagrams never end an attempt;
  only its deadline does —
  `tests/dns_sd.rs`: `a_flood_of_bogus_replies_does_not_suppress_or_poison_resolution`
  (200 wrong-id replies from the nameserver's address and 200 bogus-pod
  replies from another port, one attempt),
  `mismatched_ids_and_spoofed_sources_are_ignored` *(dns-sd)*.

## Device platform: ESP base firmware, WASM payloads, web tooling

A base firmware is flashed once onto an ESP32, ESP32-S3, ESP32-C3 or ESP8266
from the browser, or onto a Raspberry Pi Pico (RP2040) or Pico 2 (RP2350)
as a UF2 file. Afterwards, application logic ships as WASM payloads loaded over the
same serial port, through a USB-serial bridge chip (CP2102, CH340, FTDI),
the chip's native USB-Serial-JTAG, or the RP chips' USB CDC-ACM port.
Payloads talk to the world through the middleware ABI in `wasm_abi`.

Heap policy: the no-heap rule applies to the `middleware` crate. A base
firmware may use a global allocator for the payload interpreter only,
because WASM linear memory and the wasmi interpreter (`wasm-host`, `no_std`
with `alloc`) require one: on ESP `esp-alloc`; on the RP2350
(`firmware-rp/`) a fixed 384 KiB `.bss` arena behind
`linked_list_allocator`, which nothing but the interpreter allocates from,
and a payload starts only when its bound (`Limits::heap_needed`) plus
margin is free, the same check as on ESP. The RP2040 build has no payload
runtime (wasmi needs CAS atomics and `alloc::sync::Arc`, which the
Cortex-M0+ target lacks), so RP2040 firmware still has no heap at all.

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
- [x] S8 (0.6) `sys/board` (empty request) answers the board the firmware
  was built for, as `loader::BoardInfo`: `[ver u8=1][abi u16][max_image
  u32][max_memory u32][gpio u64][reserved u64][input_only u64][strapping
  u64][adc u64][pwm_max_bits u8][flags u8: runtime|usb_jtag|pwm_ref_tick]
  [uart_tx u8][uart_rx u8][n_pwm u8][pwm_pins…]`, little-endian, bit `n` of
  a mask for GPIO `n`, `pwm_pins[2·timer + channel]`; at most 96 bytes
  (40 PWM pins), so it fits the firmware's 128-byte reply. A host checks
  its pin assignment against it before sending a payload; firmware before
  0.6 answers `NotFound` —
  `loader::tests::board_info_round_trips`, `board_info_golden_bytes`, `commands_round_trip_through_parse`, `parse_rejects_bad_bodies_and_passes_app_keys`; `firmware` `dispatch::tests::s8_board_reports_profile`, `chip::tests::s8_board_info_matches_profile`; `protocol-wasm` `decodeBoardInfo` (`web/test/protocol.test.ts`: "decodes the bsp_export --wire fixture (ESP32-S3)"); `mwctl board` (`device::tests::board_is_decoded_or_not_found_on_older_firmware`, `report::tests::board_lists_pin_classes_and_pwm_table`).
- [x] S8 Publications that arrive while `request_with` or `serve_one`
  waits are kept, in order, in the link's inbox of `I` bytes (const
  generic, default `INBOX_BYTES` = one maximal raw frame + 2; each entry
  costs its raw frame + 2 bytes) and returned first by `recv`, `try_recv`,
  a `Filtered` view, `SerialRx` and `next_frame`. A publication that does
  not fit is dropped and counted (`inbox_dropped`); a buffer too small for
  a queued one keeps it; `I = 0` keeps nothing. `DELIVERY` is
  `DropNewest` with per-sender order (was `Block`): the inbox drops the
  arriving publication, and a UART without flow control drops what
  overruns its driver; nothing older is dropped for something newer. No
  gaps are reported (no sequence numbers) —
  `serial::link::tests::s8_publications_during_request_are_kept_in_order`,
  `s8_publications_during_serve_one_are_kept`,
  `s8_full_inbox_drops_the_newest_and_counts`, `s8_zero_inbox_keeps_nothing`,
  `s8_small_buffer_keeps_a_queued_publication`,
  `s8_filtered_view_and_next_frame_take_the_inbox_first`,
  `delivery_is_drop_newest_in_order`; `tests/serial.rs`:
  `s8_publications_during_a_request_reach_recv_in_order`,
  `s8_inbox_overflow_drops_the_newest`,
  `s8_request_after_a_timeout_still_works` *(serial)*.
- [x] S9 `SerialLink::with_peer` names the link's peer: received samples
  carry it as `SampleMeta::sender` and `serve_one` handlers see it as
  `RequestMeta::sender` (no principal, no deadline); without it both are
  `None` — `serial::link::tests::s9_peer_is_the_sender_of_samples_and_requests`,
  `s9_without_a_peer_meta_is_empty`.
- [x] S10 `sys/info` may end with the device's 8-byte node id
  (`DeviceInfo::node`, a `SenderId`; `MAX_INFO_LEN` 81). Every decoder
  reads the fields it knows and ignores trailing bytes, so 0.4 hosts read
  0.5 devices and 0.5 hosts read 0.4 devices (`node: None`). Checked:
  `DeviceInfo::decode` (used by `mwctl`, `protocol-wasm` and so the web
  page, the ESP8266 tests and conagotchi's `badge-mw`) —
  `loader::tests::s10_device_info_carries_a_trailing_node_id`,
  `device_info_round_trips`; `protocol-wasm` `loader::tests::info_round_trips_and_names_statuses`;
  `mwctl` `device::tests::info_decodes_a_protocol_only_esp8266` (its fake
  sends the node id), `report::tests::info_notes_protocol_only_firmware`.
- [x] S11 The serial link passes every conformance case for the traits it
  implements (request/response, pub/sub, and discovery as
  `PubSubDiscovery` over a split link), over an in-memory fake link pair
  whose pub/sub direction is lossy like a UART without flow control.
  `timeout_is_honoured` runs with the caller-side `Bounded` wrapper the
  link's docs prescribe (the link has no clock); `Gather` is not
  implemented (one peer) — `tests/serial.rs`: `k15_serial_*` *(serial)*.

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
- [x] W10 A payload that starts cannot exhaust the heap later. wasmi
  translates every function eagerly at load (`Compiled::new`), so no
  translation allocates during a call; the value stack is capped and
  allocated once in full (`Limits::max_stack_bytes`, default 4 KiB) and
  the call depth capped (`Limits::max_call_depth`, default 32), both
  trapping with a stack overflow instead of growing; the DMA copy buffer
  and subscription list are reserved at load. `Limits::heap_needed(wasm)`
  bounds the heap from compiling to the last call (the larger of the
  compile phase, image included, and the run phase, image freed; model and
  constants in `wasm_host::heap`; `Limits::load_heap_needed(image_len)` is
  the same bound for any image of that length), and covers the measured
  peak of every example payload. The ESP base firmware reads the image,
  sizes the bound from it, loads in two steps (compile, free the image,
  instantiate) and refuses a start when the free heap is below that bound
  plus `start::HEAP_MARGIN`, logging the free and needed bytes —
  `tools/payload-sim/tests/heap.rs` (every example under the badge's
  limits, and `cam_jpeg_view`'s byte-page deploy build: bound >= measured
  compile, instantiate and run peaks; `cam_jpeg_view` must decode a
  frame); `wasm-host/tests/alloc.rs` (after the deepest call, 398 calls
  allocate nothing; the frame list's growth stays within its share);
  `wasm-host/tests/runtime.rs`:
  `w10_call_depth_is_capped_by_limits`, `w10_value_stack_is_capped_by_limits`,
  `w10_compile_then_instantiate_equals_load`, `w10_heap_bound_counts_every_limit`,
  `w4_fuel_stats_record_every_call` (the first call costs no more fuel
  than the second); `heap::tests::w10_*`; esp-base `start::tests::w10_*`.
  Measured on a 32-bit host (i686: `cargo test -p payload-sim --release
  --test heap --target i686-unknown-linux-gnu`, not part of the gates),
  the bound's target width: `cam_jpeg_view`'s deploy build (25.7 KB
  image, 48 128 bytes of memory) peaks at 153.6 KB and is bounded at
  160 195 bytes (164 291 with the margin), which the badge's 188 KB free
  after boot covers with 24 KB to spare; its 64 KiB-page build is bounded
  at 177 589; `cam_view` (11.0 KB) peaks at 135.3 KB, bound 137 153. (At
  30.9 KB and a 64 KiB page, `cam_jpeg_view` needed 195 886 bytes and the
  badge refused it.)
- [x] W12 The heap bound charges linear memory at what the module can
  reach: the smaller of the host's cap and the module's declared maximum
  (`abi::memory_reach`, read from the memory section with
  `abi::memory_decl`; the cap when there is no maximum or the section is
  malformed). wasmi enforces the declared maximum on `memory.grow`, so a
  1-byte-page module with initial = maximum is charged exactly its memory
  — `abi::tests::w10_memory_decl_reads_the_declared_maximum`,
  `abi::tests::w10_memory_reach_is_the_smaller_of_cap_and_declared_maximum`;
  `wasm-host/tests/runtime.rs`:
  `w10_heap_bound_charges_the_declared_maximum_not_the_cap` (also checks
  that growing past the declared maximum fails under a larger cap);
  esp-base `start::tests::w10_holding_the_image_credits_it_back` (the
  firmware checks again with the image read, crediting its buffer).
- [x] W13 Refused and failed starts return every byte of heap: a start
  refused after the image was read, a compile error, a compiled module
  dropped, an `init` trap and a load followed by a stop, repeated 20
  times, leave exactly the heap in use before them —
  `wasm-host/tests/alloc.rs` (`refused_and_failed_starts_return_every_byte`,
  counting allocator). On the badge the free heap still moved by 1 248
  bytes between two refused starts (188 632 → 187 384, a new image
  uploaded between them, the ESP32-CAM streaming): neither the refusal,
  which allocated nothing, nor the upload path (heapless, no allocation)
  holds heap. ESP-NOW reception does: esp-radio queues each received
  frame as a `Box<[u8]>` in a `VecDeque` of up to 10 entries, whose
  capacity grows (4, 8, 16) and is never given back, and the Wi-Fi driver
  allocates its dynamic RX buffers per frame. Under a flood the free heap
  therefore moves by a few KB from one reading to the next; the start
  check's margin and headroom must cover it.
- [ ] W11 A firmware crash during a payload's run does not repeat on the
  next boot (skip the autostart after a crash). Not implemented: on the
  ESP chips `esp-backtrace` halts after a panic rather than resetting, so
  there is no boot loop, but a manual reset autostarts the same payload
  again. Doing this needs a flag that survives the reset (RTC fast memory
  or a field in the payload slot header written before the autostart and
  cleared once it has run for a while) and a panic handler that resets
  instead of halting; RTC `persistent` statics need `unsafe` access in
  esp-hal 1.2, and a flash write per boot wears the header sector, so it is
  left as a follow-up. W10 removes the known cause (heap exhaustion after
  a start).

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
  `payloads/scripts/check-reproducible.sh` (blink, echo, cam_view, cam_jpeg_view and bench, run by `check-all.sh`).
- [x] P7 The SDK cannot panic on its own paths: `LogLine` (append, decimal
  numbers dropped whole when they do not fit, `log` without re-validating
  UTF-8) and the `on_message` delivery copy use checked access only, so a
  payload that logs through them links none of core's panic formatting
  (`panic_fmt`, `Display for usize`, `str::from_utf8`): blink 5 467 → 1 972
  bytes, cam_jpeg_view 30 878 → 25 714 —
  `mw-payload` `logline::tests::*` (`a_number_that_does_not_fit_is_dropped_whole`);
  `abi-check` `p7_sdk_payloads_link_no_panic_formatting`.
- [x] P8 A benchmark payload (`bench`) measures a host's interpreter speed
  in fuel per second with workloads of known fuel cost: alternating steps,
  1 ms apart, of integer mixing over a 4 KiB table (196 564 fuel) and
  `mw-jpeg` decoding the fake camera's 240×240 frame 4 MCUs at a time
  (62 steps, 12 786 673 fuel per frame), each timed with `now_us`; every
  5 s it logs steps, average and largest step time and the implied fuel
  rate per workload, and how many `app/cam/jpeg` messages it was delivered
  (it subscribes and only counts, to load the host as `cam_jpeg_view`
  does). Its heap bound stays below `cam_jpeg_view`'s, so it starts
  wherever the viewer does —
  `bench` `tests::steps_alternate_and_a_frame_takes_the_documented_steps`,
  `tests::mix_is_deterministic_and_touches_the_table`,
  `tests::jpeg_is_the_fake_camera_frame`; `tools/payload-sim/tests/bench.rs`
  (`bench_fuel_constants_match_the_measured_steps`: the constants within
  5 % of the measured fuel, largest step 255 360;
  `bench_starts_wherever_cam_jpeg_view_starts`); `abi-check`
  `p5_forbid_unsafe_code_examples_export_the_abi`; built reproducibly with
  1-byte pages (30 720 bytes of memory) by `check-all.sh`.

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
  `firmware` core tests `dispatch::tests::info_reports_device`, `full_transfer_installs_then_start_and_stop`, `abandoned_transfer_keeps_installed_payload`, `oversized_begin_from_idle_keeps_installed_payload`, `resent_last_chunk_is_acknowledged_without_rewriting`; `store::tests::installed_image_survives_reopen_and_verifies`, `interrupted_transfer_keeps_previous_image`, `corruption_is_detected`, `limits_and_ordering_are_enforced` (RAM flash model; silicon untested).
- [x] E4 Starts the stored payload on boot and on `sys/payload/start`; stops
  on `sys/payload/stop`; publishes status and log lines —
  `dispatch::tests::start_requires_an_installed_image`, `begin_while_running_stops_and_erases`, `storage_failure_and_traps_update_status`, `start_without_runtime_is_refused_but_loader_works`, `tick_delay_handles_wrap`.
- [x] E5 Bridges payload publish/subscribe to the serial link and GPIO to
  the chip, refusing pins used by flash, USB or the console UART —
  `chip::tests::console_uart_pins_are_reserved`, `flash_psram_and_usb_pins_are_refused`, `missing_pins_are_invalid`, `esp32_input_only_pins_refuse_output`, `general_purpose_pins_are_allowed`; `dispatch::tests::publications_route_only_to_running_subscribed_payloads`. Runs payloads on ESP32 and ESP32-S3 only; the C3 has no runtime.
  From 0.5 the same two chips also bridge the ABI 3 PWM and ADC imports,
  on fixed pins that are never flash, USB or console pins (X9, X12); a pin
  held by PWM or the ADC is refused to plain `gpio_*` like an SPI or UART
  pin (X13).
- [x] E6 Produces merged flash images per chip for the web flasher —
  `firmware/build-images.sh` writes `web/public/firmware/{esp32,esp32s3,esp32c3}.bin` and `manifest.json`. It also builds the Pico UF2s for U19 (`rp2040`, `rp2350`), which are, like the `.bin` images, build output and not committed.
- [x] E14 (0.6) Each chip profile has a strapping-pin mask (ESP32 GPIO
  0/2/5/12/15, ESP32-S3 0/3/45/46, ESP32-C3 2/8/9, ESP8266 0/2/15). Strapping
  pins stay usable by payloads (`check_pin` allows them); hosts warn, as
  ESPHome does with `ignore_strapping_warning`. The PWM pins avoid them —
  `chip::tests::e14_strapping_pins_per_chip_are_allowed`; `periph::pwm::tests::x9_pwm_pins_avoid_strapping_flash_usb_console_adc1_and_i2c` (uses the mask instead of a hand list).
- [ ] E15 (0.6) Board support packages: `cargo run --example bsp_export --
  <chip>` prints the chip's `mw-bsp/1` JSON (`format, chip, firmware, abi,
  runtime, maxImage, maxMemory, console {usbJtag, uartTx, uartRx}, pins
  {gpio, reserved, inputOnly, strapping, adc1}, pwm {maxBits, refTick, table
  [{timer, channel, gpio}]}`) from the same `Profile` that `sys/board`
  serves, and `--wire` the `sys/board` body as hex (the golden fixture
  `web/test/fixtures/esp32s3.board.hex`, also used by Skygraph). A BSP is
  identified as `<chip>@<firmware>`. `build-images.sh` writes
  `<chip>.bsp.json` next to each image and an ESP Web Tools manifest whose
  builds carry `wasm` and `bsp`. `.github/workflows/release.yml` builds the
  images on a tag `esp-base-v<version>`, attaches `.bin`, `.bsp.json` and
  `manifest.json` to the release, and mirrors every release to GitHub
  Pages (`firmware/<version>/`, `firmware/latest/`), because release assets
  are not readable cross-origin from a browser. Partial: the exporter and
  the manifest writer are checked locally; the workflow is linted
  (actionlint) but has not run yet.
- [ ] E14 A busy payload or an ESP-NOW flood cannot starve the serial
  links: the controller yields to the executor once per iteration of its
  running loop, and the radio task once per received frame. Before, an
  embassy task whose awaits were always ready never returned to the
  executor: `cam_jpeg_view`'s 1 ms decode timer has always fired again by
  the time a decode call returns (and a flood keeps the radio queue
  full), so the link task never sent the start reply nor read the
  host's retry, and `mwctl upload --start` stalled (150 s seen on the
  badge, the host blocked in `tcdrain`, see H2). A refused start is not
  affected: it allocates nothing beyond the image buffer, replies through
  a signal and returns to idle. Shell code; to be verified on hardware.
- [x] E15 While a payload runs, the controller accounts for its time and
  logs it every 5 s as two `sys/log` lines: wall time inside payload
  operations and its share, export calls, fuel and fuel per second while
  busy, the longest operation (µs and fuel), the split per operation kind
  (deliveries, `on_irq`, `tick`: calls / fuel / time), the time waiting
  for input, yielding and in its own work, and the radio task's time on
  received frames with their count. On by default
  (`esp_base::perf::ENABLED`); the accounting and the line format are a
  pure core, the clock reads and the logging shell code —
  `perf::tests::window_folds_operations_per_kind_and_tracks_the_largest`,
  `summary_splits_the_wall_time_and_rates_the_fuel`,
  `cost_is_the_difference_of_the_counters_and_saturates`, `due_after_one_interval`,
  `empty_window_reports_zeros_without_dividing_by_zero`,
  `log_lines_fit_a_sys_log_line`. The lines themselves are to be read on
  hardware (see "Interpreter speed on the badge").
- [x] E16 The boot log names the SPI flash mode and clock the bootloader
  set up (`flash: DIO 40 MHz (bootloader header)`), decoded from the image
  header at flash offset 0; code runs from flash through the instruction
  cache at that speed —
  `chip::tests::flash_settings_decode_the_image_header`.
- [x] E17 Requests reach `dispatch::handle_request` as a middleware
  `Request` with its `RequestMeta` (empty from a serial port; from ESP-NOW
  (O8) the sender's MAC as `SenderId`; no `sys/*` decision depends on it,
  the sender being unauthenticated — the over-the-air policy is
  `esp_base::ota`, O6), and
  `sys/info` ends with the node id from the chip's base MAC
  (`Context::node`; the ESP8266 build sends none) —
  `dispatch::tests::e17_info_ends_with_the_node_id_and_old_decoders_ignore_it`,
  `info_reports_device`.
- [x] E18 The radio receives `app/...` and `disc/...` keys
  (`radio::FILTERS`, the transport's two filter slots); `sys/...` never.
  A payload sends on `app/...` only: its publishes (`goes_over_air`, also
  checked again in the radio shell's `send`) and its source routes
  (`carries_routes`) refuse `disc/...`, which is reserved for the radio
  task's own beacon, so a payload cannot forge discovery beacons.
  Received beacons go to the directory, not to the payload. The only way
  `sys/...` crosses the radio is the separate request path of the
  over-the-air update (`MR` frames, built only by the radio task, admitted
  by `esp_base::ota`, O6/O16); a payload's publish is always an `MW`
  sample, even one whose payload is a whole `MR` frame, so it reaches
  neither discovery nor the request path —
  `radio::tests::e18_beacons_are_received_but_payloads_cannot_send_them`,
  `e18_o6_a_payload_reaches_neither_discovery_nor_the_request_path`,
  `r11_r12_only_app_keys_go_over_the_air`.
- [x] E19 While a camera source route runs, the radio task broadcasts a
  `disc/cam/jpeg` beacon (instance `cam-` + the MAC's last three bytes in
  hex, node id the MAC, endpoint local, TXT `key=<route key>`, TTL 30 s)
  at the route's start and every 15 s, and one goodbye when it stops:
  under 100 bytes per 15 s. Pure core `radio::disc` (`CamBeacon`,
  `cam_beacon`, the `discovery::beacon` codec); the shell sends it —
  `radio::disc::tests::e19_cam_beacon_names_kind_instance_node_and_route`,
  `e19_cam_beacon_runs_with_the_camera_route`;
  `route::sources::tests::r15_camera_routes_ask_for_a_capture_then_send_its_chunks`
  (`camera_key`). On hardware (ESP32-CAM) still to be seen.
- [x] E20 Every device keeps a `discovery::Directory` of 4 records fed
  from the `disc/...` beacons it receives (a missing node id is taken from
  the sample's sender) and logs on `sys/log`
  `disc: up <kind> <instance> node <16 hex> ttl <n> s (<k> known)`,
  `disc: updated ...`, `disc: down <kind> <instance> (goodbye, <k> known)`,
  and `disc: down <kind> <instance> (expired)` (refusals, E21, are not logged);
  refreshes are silent. The conagotchi badge runs this shell, so its log
  shows the ESP32-CAM's `disc: up cam/jpeg cam-...` line —
  `radio::disc::tests::e20_neighbours_log_up_refresh_down_and_expiry`,
  `e20_neighbours_fill_in_the_sender_and_ignore_non_beacons`. The line on
  the badge is to be read on hardware.
- [x] E21 Directory records are bound to the frame's sender (the ESP-NOW
  source MAC): the sender is the record's node and a beacon declaring a
  different node is refused; a record is updated or withdrawn only by the
  sender that created it; one sender holds at most `PER_SENDER` (2) of
  the 4 records, and a new record finding the directory full is refused,
  never evicting another's. Refusals are counted (`Neighbours::refused`),
  not logged. The source MAC is not authenticated, so this stops one node
  claiming another's identity or records, not a radio forging MACs —
  `radio::disc::tests::e21_records_are_bound_to_the_frame_sender`,
  `e21_one_sender_cannot_fill_the_directory`.
- [x] E23 The serial port tasks run their links with no inbox
  (`shell::link::INBOX` = 0): they read with `next_frame` and never wait
  in `request_with`/`serve_one`, so nothing could be queued, and each
  port saves 1 KiB of `.bss` (the ESP32-S3 main stack is what `.bss`
  leaves). With the default inbox the conagotchi badge's `.stack` fell
  to 40 920 B, under its 40 KiB check; with none it is 45 032 B —
  conagotchi `scripts/check.sh` "firmware main stack" step (2026-10-29),
  `firmware/build.sh esp32s3`.
- [x] E24 I2C bus 0 for payloads on the ESP32-S3 (ABI 3), the ESP32's
  driver and claims: SDA GPIO 5, SCL GPIO 6 (`chip::ESP32S3`'s `i2c0`, a
  board constant: no strapping, flash/PSRAM, USB or UART0 pin, free on the
  ESP32-S3-DevKitC-1 and on the conagotchi badge), clocks up to 400 kHz
  (bus 1 and faster clocks `Unsupported`); the pins are exclusive to the
  bus while open and released on stop. On both chips each byte has a
  software timeout of two byte times plus 100 µs (esp-hal's defaults left
  a stuck bus on the S3 unbounded), so a 64 + 64-byte transfer returns
  within about 37 ms at 100 kHz and 255 ms at 10 kHz. The ESP32-C3 and
  ESP8266 run no payloads, so they have no I2C or SPI —
  `chip::tests::e24_esp32s3_maps_i2c_bus_0_to_gpio_5_and_6`;
  `periph::claims::tests::e24_esp32s3_i2c_bus_0_holds_gpio_5_and_6_exclusively`;
  `periph::i2c::tests::e24_byte_timeout_is_two_byte_times_plus_slack`,
  `e24_a_full_transfer_is_bounded`, `e24_open_plan_accepts_bus_0_up_to_fast_mode`;
  `firmware/build.sh esp32s3`. Transfers on S3 hardware untested.
- [x] E22 The `disc:` log lines print radio-supplied kinds and instances
  with every byte outside printable ASCII, and `\`, as `\xNN`, so a
  beacon cannot inject terminal escapes or line breaks into `sys/log` —
  `radio::disc::tests::e22_log_lines_escape_radio_supplied_text`.

#### Interpreter speed on the badge

`cam_jpeg_view` decoded a 240×240 frame in 9 761 ms on the badge while an
ESP32-CAM flooded it with about 290 ESP-NOW frames a second: about 1.4 M
fuel per second, for 13.2 M fuel per frame measured in `payload-sim`.
Findings from the code and the built images, most likely cause first:

1. **Code runs from flash at espflash's defaults, DIO 40 MHz, and the
   radio evicts the interpreter from the instruction cache.** Neither the
   badge's runner nor `build-images.sh` passes `--flash-mode` or
   `--flash-freq`, and espflash 4 defaults the ESP32-S3 to DIO at 40 MHz:
   a 32-byte cache line costs about 150 SPI clocks, some 900 CPU cycles at
   240 MHz, where QIO at 80 MHz costs about a quarter of that. The
   interpreter's dispatch loop calls one of about 1 100 handlers per
   instruction, spread over 113 KB of flash; the i32, branch, copy and
   memory handlers alone are about 32 KB, the size of the whole
   instruction cache (already at its maximum: 32 KiB, 8 ways, 32-byte
   lines, esp-hal's default). Every received frame runs the Wi-Fi
   driver's receive path, much of it from flash, between interpreter
   instructions; refilling a 20 KB hot set at DIO 40 MHz takes about 2 ms.
   Placing the hot handlers in IRAM (esp-hal's `rwtext_hook.x`, enabled
   with `ESP_HAL_CONFIG_USE_RWTEXT_LD_HOOK`) is possible by section name,
   but IRAM comes out of the same SRAM as the main stack and the heap:
   the badge has about 7 KB above its 40 KiB stack floor, far from the
   32 KB the integer handlers need, without shrinking the payload heap.
2. **Deliveries of the flood.** Every received frame on a subscribed key
   is two export calls (`mw_alloc`, `on_message`) and a copy into linear
   memory, even when the payload then drops it; at 290 frames a second
   that is 580 calls a second on the main task between decode steps.
   The `perf:` lines' `msg` column measures it.
3. **Pacing (computed, small).** A frame is 1 start and 61 decode calls
   with a 1 ms one-shot gap between them, plus 105 panel transfers of at
   most 3 840 bytes at 40 MHz SPI (115 200 bytes, about 23 ms): about
   90 ms of a frame's wall time is gaps and transfers, under 1 % of
   9 761 ms.
4. **Build profile (ruled out).** The badge builds wasmi at
   `opt-level = "z"`, esp-base at `"s"`; at `3` the handlers come out
   the same size (the i32 add handlers byte for byte) and the image grows
   by 145 KB, so the interpreter's inner loop does not depend on it. No
   debug assertions or overflow checks in either release profile.
5. **Per-call host overhead (small).** Each export call looks the
   function up by name and refuels; next to a 200 000-fuel decode step
   this is noise, and for deliveries it is part of item 2.

The decisive measurements are the `bench` payload's rates with the radio
quiet and flooded, the `perf:` lines (fuel per second inside calls, and
where the rest of the time goes) and the same again after flashing with
`--flash-mode qio --flash-freq 80mhz` (`flash:` at boot shows which is in
effect).

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
  `missing_pins_are_invalid`, `general_purpose_pins_are_allowed`,
  `max_image_fits_one_payload_slot`.
- [x] E8 Answers `sys/info`, installs payloads into flash at 0x100000 (above
  the 1 MB code window) with CRC-32 verification, keeps them across reboot,
  and refuses start. The 256 KiB region holds two 128 KiB slots (E12), so
  `sys/info` reports a capacity of 126976 bytes (124 KiB) and a rejected
  upload keeps the previous image —
  host: `controller::tests::fresh_boot_reports_empty_esp8266_without_runtime`, `upload_installs_and_survives_reboot`, `start_is_refused_and_stop_is_a_no_op`, `wrong_crc_commit_on_empty_flash_leaves_no_image`, `rejected_upload_keeps_previous_image`, `uploads_alternate_slots_and_newest_wins`, `corrupted_stored_image_boots_empty`, `largest_image_fits_a_slot`, `oversized_and_unknown_requests_are_refused`, `info_request_over_the_link`.
  Hardware (ESP8266EX, 4 MB flash, 2026-10-25, single-slot firmware before
  E12) with `mwctl`: `info` reported esp8266, `0.1.0+no-wasm`, ABI 2,
  capacity 258048; `upload echo.wasm` installed (status Stopped); after
  `mwctl reset` the boot log reported the stored image and `info` still said
  Stopped; `start` answered NotFound; `upload --corrupt-chunk 3` failed at
  commit with InvalidRequest and, with one slot, left status Empty. A
  252 KiB image also installed and survived reboot. The two-slot firmware
  was then verified on the board; see E13.
- [x] E9 The flash driver satisfies `NorFlash` semantics (4 KiB erase one
  sector per HAL call, 4-byte writes split at 256-byte pages, region-relative
  bounds) — `flash::tests::erased_reads_ff_and_writes_and_like_nor`, `addresses_are_region_relative`, `erase_issues_one_call_per_sector`, `writes_split_at_page_boundaries`, `aligned_page_sized_write_is_one_program`, `misaligned_requests_are_refused_without_device_access`, `out_of_region_requests_are_refused`, `whole_region_erases`, `device_failures_surface_as_other` (RAM model, `firmware-esp8266/src/flash/ram.rs`).
- [x] E10 `firmware/build-images.sh` emits `esp8266.bin` (merged, offset 0x0)
  and its manifest entry with `"wasm": false` —
  full four-chip `firmware/build-images.sh` run (2026-10-25) wrote
  `web/public/firmware/esp8266.bin` (78508 bytes, via
  `firmware-esp8266/image.sh`) and the `esp8266` manifest entry; the same
  merged image, written by `firmware-esp8266/flash.sh`, boots on the board.
- [x] E11 The hardware watchdog (26.8 s stage, fed every super-loop pass)
  resets the chip when starved, and the chip boots the firmware again —
  hardware (ESP8266EX, 2026-10-25), image built with the opt-in `wdt-test`
  feature (`firmware-esp8266/build.sh --features wdt-test`, then `flash.sh`),
  which runs normally for 5 s after boot, logs `wdt-test: starving watchdog`
  and stops feeding. A timestamped pyserial capture (DTR and RTS deasserted)
  over 130 s saw four resets, each with the ROM line `rst cause:4, boot
  mode:(3,7)` and `wdt reset`, followed by the `sys/log` banner
  `esp-base 0.1.0+no-wasm on esp8266`. Reset to reset took 31.865 to
  31.867 s, and the starve log to the next reset took 26.843 to 26.844 s,
  matching the 26.84 s stage. After a watchdog reset the PLL stays set up,
  so the ROM log arrives at 115200 baud, not the 74880 baud of a power-on
  boot. Rebuilt with plain `build.sh` (merged image byte-identical to the
  one built before the feature existed) and reflashed: `mwctl info`
  answered, and a 90 s capture saw no output and no reset.
- [x] E12 The payload store keeps two slots, so a failed upload keeps the
  previous image. The payload partition (ESP32 family) or region (ESP8266)
  is split into two equal slots, each a header sector (magic, length,
  CRC-32, generation) followed by the image; for 256 KiB that is 124 KiB
  per slot, which every chip's `max_image` fits. `begin` erases and fills
  only the inactive slot, and commit re-checks the written image's CRC
  before writing that slot's header, magic word last. Boot picks the valid
  slot (header decodes, image CRC matches) with the newer generation, by
  wrapping serial-number comparison, slot A on a tie; with neither valid
  the store is empty. The dispatcher returns to `Stopped`, not `Empty`,
  when a transfer or storage action fails while an image is installed —
  `store::tests::first_install_on_empty_flash_uses_slot_a`, `wrong_crc_commit_keeps_old_image_after_reboot`, `interrupted_transfer_keeps_previous_image`, `power_loss_at_any_flash_operation_keeps_old_or_new_image` (a torn erase or write injected at every flash operation in turn, with one, two or no prior images), `generations_alternate_slots_across_installs`, `generation_wraps_past_u32_max`, `corrupt_newest_slot_falls_back_to_older`, `both_slots_corrupt_boots_empty`, `limits_and_ordering_are_enforced`; `image::tests::region_splits_into_two_slots`, `generations_compare_across_wrap`, `newest_valid_slot_wins_and_ties_go_to_a`, `header_round_trips_and_rejects_erased_or_oversized`; `dispatch::tests::abandoned_transfer_keeps_installed_payload`, `failed_transfer_after_first_install_keeps_it`, `storage_failure_and_traps_update_status`; `chip::tests::max_image_fits_one_payload_slot` (RAM flash model).
- [x] E13 On the ESP8266 board, `mwctl upload ... --corrupt-chunk 3` after
  a good upload fails at commit and leaves the previous image installed
  (status Stopped, and still stored after `mwctl reset`), and `info`
  reports capacity 126976 — hardware (ESP8266EX, 2026-10-25), two-slot
  firmware (80004-byte merged image via `firmware-esp8266/flash.sh`):
  `info` reported capacity 126976 and status Empty; `upload echo.wasm`
  (5167 bytes) installed, status Stopped; `upload echo.wasm
  --corrupt-chunk 3` failed at step 13/13 with `sys/loader/commit`
  InvalidRequest and status returned to Stopped; after `mwctl reset` the
  boot log reported `stored payload: 5167 bytes, crc dd2098fa` and `info`
  said Stopped. A second good upload (into the other slot) then installed
  and survived `mwctl reset`, and a repeated corrupt upload again left
  status Stopped.

### RP base firmware (`firmware-rp/`)

The base firmware for the Raspberry Pi Pico (RP2040) and Pico 2 (RP2350A,
Arm cores), one source tree with a cargo feature per chip, on embassy-rp.
It speaks the protocol of the ESP base firmware, unchanged, over the
chip's USB port as one CDC-ACM interface: COBS frames, the `sys/info`,
`sys/loader/*`, `sys/payload/*`, `sys/trust/*` and `sys/log` keys, signed
payloads, so `mwctl --port /dev/ttyACM0` and the web console work against
it as they do against esp-base.

Shared with esp-base rather than copied: the request decisions and storage
come from the `esp-base` library crate with its default features off and
`trust` on, the way the ESP8266 firmware uses it: `esp_base::dispatch`
(request table, loader state machine, payload lifecycle, trust rules),
`esp_base::store` and `esp_base::image` (the two-slot payload store),
`esp_base::trust_store` (the pinned key), `esp_base::start` (the heap
check) and `esp_base::periph::timers` (the payload timer table). None of
them changed, so esp-base's own gates are untouched. What esp-base keeps in
its private shell (U1's finding: the shell is not exported) is ESP-specific
and is written again for embassy-rp, kept thin: a one-port message bus, the
CDC link task and the controller loop (`firmware-rp/src/shell/`). The RP
pure core (`rp_base`, host tested) adds the flash maps, the chip profiles
and pin policy, the request service that performs the storage effects over
any NOR flash, the `sys/fw/*` seam and, on the RP2350, the A/B firmware
updater that serves it (`rp_base::ota`, see "A/B firmware OTA (RP2350)"),
the one-sector erase adapter and the untranslated-read window, the USB
framing rules and the heap budget.

Tasks: the USB device task, the link task (middleware's `SerialLink` over
the CDC halves joined as a `Duplex`), and the controller on the main task,
which owns the service (dispatcher state, payload store, trust record) and
the running payload. They meet only through the `'static` bus.

Flash maps (offsets from the start of flash; every region whole 4 KiB
sectors, checked by `layout::FlashMap::check`):

| board | partition table | firmware | firmware slot B | update record | payload store | trust | unused |
|---|---|---|---|---|---|---|---|
| Pico, RP2040, 2 MiB | — | 0x000000, 1.5 MiB (boot2, then the image) | — | — | 0x180000, 256 KiB | 0x1C0000, 8 KiB | 248 KiB |
| Pico 2, RP2350, 4 MiB | 0x000000, 8 KiB | 0x002000, 1528 KiB (slot A, partition 0) | 0x180000, 1528 KiB (partition 1, linked to A) | 0x2FE000, 8 KiB | 0x300000, 256 KiB | 0x340000, 8 KiB | 760 KiB |

The payload store is the ESP partition's size, so both report the same
124 KiB slot capacity (capped by the chip's `max_image`). The RP2350's
payload store and trust record kept their offsets when the A/B layout
came, so a board flashed before keeps its payload and its pinned key. The
RP2350 image is linked at 0x10000000 for one slot (`memory-rp2350.x`
`FLASH` = 1528 KiB); the boot ROM maps the slot it boots there through
the QMI's address translation (window 0), so the same image runs from
either slot.

Under that translation 0x10000000 is the running slot, and anything past
its end faults, while the boot ROM's erase and program functions (which
embassy-rp's flash driver calls) take physical offsets. So the shell maps
the whole flash, untranslated, at XIP window 1 (0x10400000, QMI
`ATRANS1` base 0, 4 MiB; a safe PAC register write) and every flash access
of the store, the trust record and the updater goes through
`rp_base::flash::Window`, which reads at 4 MiB + offset (the driver is
declared 8 MiB for that) and erases and writes at the offset: every offset
in the table above is physical for reads, erases and writes alike. On a
board without a partition table nothing is translated and the window is
the identity, so the same build works there (without the updater).

RAM: the RP2040 build has no heap (no runtime) and 22 KB of static RAM,
leaving about 242 KiB of stack. The RP2350 build holds a 384 KiB heap arena
in `.bss` (static RAM 436 KB in all, the firmware updater's 5 KiB sector
stash, the 4 KiB SPI transfer buffer and the main task's larger future included), leaving about 94 KiB
for the main stack, above the 31 KiB a payload load needs on the ESP32-S3. Payloads get
128 KiB of linear memory and images up to 64 KiB; on 32 bits the heap
bound of a 64 KiB image at the full memory cap is 344 KiB (296 KiB while
compiling), so every image the store accepts is admitted. The byte-page
builds of `blink` (1 992 B) and `echo` (2 700 B, 16 KiB memory,
`payloads/scripts/build-byte-pages.sh`) are bounded at 63 and 65 KB (67
and 69 KB with the margin); their 64 KiB-page builds, charged at the
128 KiB cap, at 178 and 180 KB. The byte-page builds would also fit the
RP2040's RAM; what keeps payloads off it is the missing atomics, not
memory.

Payload ABI on the RP2350 (the RP2040 runs no payloads):

| ABI call | RP base |
|---|---|
| `log`, `publish`, `subscribe`, `set_tick`, `now_ms`, `now_us` | yes |
| `gpio_output`, `gpio_input`, `gpio_write`, `gpio_read` | yes: GPIO 0–22 and 25 (LED); 24 (VBUS sense) input only; 23 and 29 reserved; `Busy` on a pin an SPI or I2C bus holds |
| `timer_start`, `timer_stop`, `irq_overflow` | yes: four software timers fired by the controller loop, delivered to `on_irq` |
| `adc_read` | yes: GPIO 26–28 (ADC0–ADC2), 12-bit; those pins are analog only |
| `spi_open`, `spi_transfer`, `dma_read` (`Spi0`) | yes: bus 0 on SPI0 or SPI1, whichever the payload's pins select (pins checked against the GPIO function table); 1.93 kHz (RP2040) / 2.31 kHz (RP2350) to 40 MHz, never faster than asked; modes 0–3; CS any output pin, low for the whole transfer; up to 4092 bytes pumped through the FIFOs by the controller loop (no DMA), completion as `DmaDone` |
| `i2c_open`, `i2c_transfer` | yes: bus 0 SDA GPIO 4 / SCL GPIO 5, bus 1 SDA GPIO 6 / SCL GPIO 7 (internal pull-ups on), 10 kHz–1 MHz; blocking, each transfer time bounded; NACK, arbitration loss and timeout are `Transport`; address-only probes `Unsupported` |
| `gpio_irq` | `Unsupported` |
| `uart_open`, `uart_write`, `uart_read`, `dma_read` (`Uart1Tx`) | `Unsupported` |
| `pwm_open`, `pwm_set`, `pwm_stop` | `Unsupported` (no pins bound to PWM on this board map) |
| `route_source`, `route_sink`, `route_stop`, `route_stat` | `Unsupported` |

SPI and I2C on the RP base. Both chips route each peripheral signal to
fixed GPIOs (functions F1 and F3 of the datasheets' GPIO function tables,
identical for GPIO 0–29 on the RP2040 and RP2350): SPI0 has SCK on 2, 6,
18, 22, TX (MOSI) on 3, 7, 19, 23 and RX (MISO) on 0, 4, 16, 20; SPI1 SCK
on 10, 14, 26, TX on 11, 15, 27, RX on 8, 12, 24, 28; I2C0 SDA on every
pin `n` with `n % 4 == 0`, I2C1 SDA on `n % 4 == 2`, SCL one above. The
ABI has one SPI bus, so bus 0 lands on whichever controller its pins name;
the board maps I2C bus 0 and 1 to the Pico pinout's defaults (GP4/GP5,
GP6/GP7). The controllers are driven through embassy-rp's PAC, not its
drivers: those take pins by type, which the pin bank (one `Flex` per
payload pin for the firmware's life) cannot give, and their blocking loops
are unbounded on a stuck bus. A bus takes a pin by switching its function
select and gives it back as an input without pull. Pins are exclusive both
ways (`rp_base::periph::claims`): a bus cannot take a pin another bus holds
or one the payload configured as GPIO, and `gpio_*` on a bus pin is
`Busy`; reserved and ADC pins are `Unsupported` for every bus. (esp-base
answers a pin held by another peripheral with `InvalidArgument` and lets a
peripheral take a GPIO pin.) An I2C transfer gives up after twice its bus
time plus 2 ms (at most 237 ms) and an SPI transfer after twice its bus
time plus 100 ms; a fault resets the controller. A stop, trap or upload
holds every controller in reset and frees every pin.

Known gaps: neither board has a presence button wired, so there is no
re-key window (a claimed device is re-keyed by erasing the trust region,
for example `picotool erase` of its two sectors); the RP2040 stores
payloads but cannot run them until wasmi supports targets without CAS
atomics; nothing has run on silicon yet.

- [x] V1 `sys/info` reports the chip (`rp2040`, `rp2350`), the firmware
  version (`+no-wasm` on the RP2040), the store capacity and, as node id,
  the chip's 64-bit unique id (the flash unique id on the RP2040, the OTP
  chip id on the RP2350), which is also the USB serial number in hex —
  `service::tests::v1_info_reports_the_rp_chip_capacity_and_node_id`,
  `node::tests::v1_node_id_and_usb_serial_come_from_the_unique_id`.
- [x] V2 The flash maps above are sector aligned, inside the flash, free of
  overlaps, start at offset 0 (the firmware, or on the RP2350 the
  partition table), hold the 8 KiB trust record and a 256 KiB payload store
  (124 KiB slots); the RP2350 has two equal firmware slots, the partition
  table and the two-sector update record outside both slots, and only all
  three together; the `FLASH` length of `memory-rp2040.x` and
  `memory-rp2350.x` equals the map's linked firmware size —
  `layout::tests::v2_both_maps_are_valid_and_keep_the_esp_payload_capacity`,
  `y13_rp2350_has_a_table_two_equal_slots_and_the_record_outside_them`,
  `v2_memory_x_flash_lengths_match_the_maps`, `v2_check_refuses_broken_maps`.
- [x] V3 Payloads go into the two-slot store with esp-base's semantics: an
  upload survives a reboot and starts; a serial `begin` stops a running
  payload; a commit with a wrong CRC, or a transfer cut by power loss,
  keeps the previous image; an image larger than a slot is refused —
  `service::tests::v3_an_upload_survives_a_reboot_and_runs_on_start`,
  `v3_a_cut_or_spoiled_transfer_keeps_the_previous_image`,
  `v3_an_image_larger_than_a_slot_is_refused` (RAM flash model with
  power-loss injection).
- [x] V4 Signed payloads: an unclaimed device accepts any commit; a claim
  pins the key and survives reboots; once claimed, unsigned, foreign-key
  and replayed commits are denied with the previous image kept; a signed
  commit raises `min_version`, which reaches flash before the new slot
  header (a power cut in between keeps the old image with the raised
  `min_version`, and RAM adopts it); a second claim is denied —
  `service::tests::v4_a_pinned_key_and_its_min_version_survive_reboots`,
  `v4_min_version_reaches_flash_before_the_new_header`.
- [x] V5 Flash erases are issued one 4 KiB sector per driver call, so
  interrupts (USB) are serviced between sectors; on the RP2350 reads go
  through the untranslated window and every access is bounded by the
  physical flash — `flash::tests::v5_erases_one_sector_per_driver_call`,
  `y12_window_reads_through_the_untranslated_alias_and_writes_physically`.
- [x] V6 `sys/fw/*` belongs to the firmware-update backend, a generic
  `FwUpdate<F>` parameter of the service that gets the flash (lent by the
  payload store), the node id, the root of trust and the self-test with
  each request: the RP2350 build's backend is the A/B updater (Y12); the
  `Unsupported` stub answers `Unavailable`; without `fw-update` (RP2040)
  the keys are `NotFound`; none touches the payload loader, store or
  status — `fw_update::tests::v6_the_reserved_keys_are_under_the_prefix`,
  `service::tests::v6_sys_fw_keys_go_to_the_update_backend_only`,
  `ota::tests::y12_requests_reach_the_updater_and_unknown_fw_keys_are_not_found`.
- [x] V7 Heap plan: the RP2040 has no heap; the RP2350's 384 KiB arena
  holds linear memory, the largest image and the interpreter reserve and
  leaves at least 128 KiB of SRAM for statics and stack; its heap check
  admits images up to the 64 KiB cap at either 16 KiB (byte pages) or
  128 KiB of memory (on the 64-bit host bound, which is larger, at least
  40 KiB), and a smaller heap admits less —
  `chip::tests::v7_heap_plans_fit_and_images_fit_a_slot`,
  `budget::tests::v7_the_rp2350_heap_admits_images_up_to_its_cap`,
  `v7_a_smaller_heap_admits_less`.
- [x] V8 Pins follow the Pico board map: pins the chip lacks are
  `InvalidArgument`, 23 and 26–29 `Unsupported` as GPIO, 24 input only;
  `adc_read` maps GPIO 26–28 to ADC0–ADC2; GPIO modes follow esp-base's
  rules (write needs an output, read of an unconfigured pin is
  `InvalidArgument`) and every configured pin is listed for release —
  `chip::tests::v8_pins_follow_the_pico_board_map`,
  `v8_adc_reads_gpio_26_to_28`, `pins::tests::v8_gpio_modes_follow_the_esp_base_rules`.
- [x] V9 `mwctl`, unchanged, drives the service over a byte stream:
  `info`, `upload --start`, `stop`, `trust claim`, signed and refused
  uploads, with the image on flash after a reboot —
  `service::wire::v9_mwctl_info_upload_and_start_work_unchanged`,
  `v9_mwctl_signed_upload_and_trust_work_unchanged` (mwctl's `Device` over
  an in-process transport into the real service).
- [x] V10 USB CDC framing: a flushed write whose last packet was a full 64
  bytes is ended with a zero-length packet; after a send times out
  (nobody reading the port) publications are dropped until a frame
  arrives — `usb::tests::v10_a_frame_ending_on_a_full_packet_gets_a_zero_length_packet`,
  `v10_publications_are_dropped_while_the_host_is_away`.
- [x] V11 The RP2040 build stores payloads but refuses `sys/payload/start`
  (`NotFound`), and `mwctl` sees it as a `+no-wasm` device —
  `service::tests::v11_without_the_runtime_payloads_are_stored_but_not_started`,
  `service::wire::v11_mwctl_sees_a_no_wasm_rp2040`.
- [x] V13 The service fails closed when the trust record cannot be read
  at boot (either sector unreadable, or the region unusable): payload
  `begin`, `chunk`, `commit` (signed or not) and `sys/trust/claim` answer
  `Unavailable` and change nothing, instead of being decided as on an
  unclaimed device (esp-base's `TrustStore::open` reads an unreadable
  sector as holding no record, so the service probes both sectors first);
  `sys/info`, `sys/trust/info`, start and stop still answer; the self-test
  fails —
  `service::tests::v13_an_unreadable_trust_record_refuses_payload_installs_and_claims`;
  `ota::tests::y16_an_unreadable_trust_record_refuses_firmware_installs`
  (loader part).
- [x] V14 The `periph_demo` payload (one SPI loopback transfer and one
  register read from a configured I2C address, no address scanning) runs
  in `payload-sim` against its SPI loopback and I2C register device, and
  its byte-page build (16 KiB memory) fits the Pico 2 heap —
  `tools/payload-sim/tests/periph_demo.rs`
  (`v14_periph_demo_runs_spi_loopback_and_i2c_read_in_the_simulator`,
  `v14_periph_demo_logs_a_mismatch_when_miso_disagrees`; 4 922-byte image);
  `budget::tests::v7_the_rp2350_heap_admits_images_up_to_its_cap`
  (byte-page images up to the 64 KiB cap at 16 KiB memory).
- [x] V15 SPI pins follow the chips' GPIO function tables: SCK, MOSI and
  MISO must carry their signal and sit on one controller (SPI0 or SPI1);
  CS may be any output pin; a pin without the signal or signals on two
  controllers are `Unsupported`, the board policy applying first, for the
  RP2040 and the RP2350 alike —
  `periph::func::tests::v15_function_tables_match_the_datasheets`,
  `v15_spi_pins_resolve_onto_one_block_on_both_chips`.
- [x] V16 I2C buses 0 and 1 use GPIO 4/5 and 6/7, which carry I2C0 and
  I2C1 in the function table; every ABI clock (10 kHz–1 MHz) has a timing
  within 1 % at 125 and 150 MHz `clk_peri`, others are refused; a transfer
  is the writes, a repeated start and the reads with one stop at the end;
  address-only probes are `Unsupported`; every transfer has a time bound
  (at most 237 ms); NACK, arbitration loss, other aborts and the timeout
  are `Transport` —
  `chip::tests::v16_i2c_buses_map_to_the_pico_default_pins`;
  `periph::i2c::tests::v16_every_abi_clock_has_a_timing_on_both_chips`,
  `v16_clocks_outside_the_abi_or_the_divider_are_refused`,
  `v16_transfers_are_writes_then_reads_after_a_repeated_start`,
  `v16_every_transfer_is_bounded_in_time`, `v16_every_fault_is_transport`.
- [x] V17 The SPI divider gives the fastest clock not above the request
  (requests below the slowest divider are `Unsupported`), modes map to
  polarity and phase, the FIFO pump moves every byte in bounded steps
  with never more than 8 bytes in flight and receives in place, and every
  transfer has a time bound —
  `periph::spi::tests::v17_the_divider_never_exceeds_the_request_and_is_the_closest`,
  `v17_clocks_below_the_slowest_divider_are_unsupported`,
  `v17_modes_map_to_polarity_and_phase`,
  `v17_the_pump_moves_every_byte_in_bounded_steps`,
  `v17_every_transfer_is_bounded_in_time`.
- [x] V18 Bus pins are exclusive: the SPI bus and each I2C bus hold their
  pins, `gpio_*` on them is `Busy`, a bus cannot take another bus's pin or
  a payload GPIO pin (`Busy`), reserved and ADC pins never become bus pins,
  re-opening SPI replaces its pins and release frees one owner —
  `periph::claims::tests::v18_buses_hold_their_pins_and_gpio_is_busy_on_them`,
  `v18_pins_are_exclusive_between_buses_and_gpio`,
  `v18_reopening_spi_replaces_its_pins_and_release_frees_one_owner`,
  `v18_analog_and_reserved_pins_never_become_bus_pins`. The shell's
  release on stop (controllers in reset, pins back to inputs) is in
  `shell/device.rs` and untested on hardware.
- [ ] V19 On a Pico 2, `periph_demo` with GP19 jumpered to GP16 logs
  `spi loopback ok` every period, and with an I2C target on GP4/GP5 at the
  configured address logs the register's value; a missing target logs
  `Transport`; after `mwctl stop` GP4/5 and GP16–19 are GPIO inputs again.
  Not yet run on hardware.
- [ ] V12 On a Pico and a Pico 2 flashed with the UF2 images (the Pico 2
  with its factory UF2), the CDC port enumerates, `mwctl info` reports the
  chip and node id, an upload survives a power cycle, and on the Pico 2
  `blink` toggles GPIO 8 and publishes `app/blink/state`. Not yet run on
  hardware.

### Host tool (`tools/mwctl`)

`mwctl --port <PATH> info|upload|start|stop|monitor|reset` drives a device
over one named serial port, synchronously, with the same pure frame and
loader code as the firmware. With `--espnow <MAC>` the port is the
USB↔ESP-NOW bridge and the request reaches that device over the air (see
"Over-the-air payload update"). It never enumerates ports. It opens the port
without resetting the board (verified on the CH340 board: no boot output on
open).

- [x] H1 (was T1 before 0.4, renamed so T* names the trust requirements)
  Protocol logic is tested against an in-memory device running the
  real splitter and loader —
  `device::tests::info_decodes_a_protocol_only_esp8266`, `upload_happy_path_installs_reports_progress_and_starts`, `dropped_chunk_reply_is_retried_and_the_device_deduplicates`, `dropped_request_and_late_reply_are_handled`, `exhausted_retries_time_out`, `corrupt_chunk_makes_commit_fail_on_the_device`, `start_without_runtime_is_a_clear_error`, `oversized_image_is_refused_at_begin`, `boot_garbage_before_the_first_reply_needs_no_retry`; `link::tests::garbage_without_delimiter_before_a_frame_is_skipped`, `garbage_with_delimiters_is_noise_and_frames_still_decode`; `plan::tests::*`, `cli::tests::*`.
- [ ] H2 A request always ends within its attempts' timeouts, whatever
  the device does: the serial transport writes without `flush`
  (serialport's is `tcdrain`, which waits with no timeout until the
  device has taken every byte, so a device that stops reading its
  USB-Serial-JTAG port blocked mwctl indefinitely), and a refused
  `start` keeps listening 2 s for the device's explanation on `sys/log`
  before reporting the error. Shell code (`tools/mwctl/src/port.rs`,
  `main.rs`); to be verified on hardware.
- [x] H3 `mwctl monitor` prints `sys/log` lines with control characters
  escaped (`\u{1b}`, `\r`, ...), never passed to the terminal, as
  `[pub]` lines already did by switching to hex; printable Unicode is
  kept — `report::tests::events_are_one_readable_line`.

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
  `npm test` in `web/` (123 tests; `web/check.sh` runs it and the build) and `cargo test -p protocol-wasm` (17 tests).
- [x] U8 The manifest carries each image's `wasm` flag (absent means `true`)
  and the UI marks protocol-only images; the device page disables "start
  after upload" and Start for `+no-wasm` firmware —
  `web/test/manifest.test.ts`: "parses the wasm runtime flag, defaulting to true when absent"; `web/test/upload.test.ts`: "recognises protocol-only firmware by its +no-wasm version suffix".
- [x] U9 A Blocks tab (Blockly) generates one AssemblyScript payload that
  imports only the `mw` functions its blocks use, exports `init`, `tick`,
  `mw_alloc` + `on_message` and `on_irq` only when needed, never allocates
  (fixed 512-byte receive and transmit buffers, integer-only values,
  loops with constant bounds) and shows the code read-only —
  `web/test/blocks.generator.test.ts`: golden files `web/test/fixtures/blocks/*.as` ("blink", "echo", "chase", "logic", "irq" ... "matches ... and compiles"), "imports only what the blocks use and exports only the handlers needed", "uses fixed buffers and never allocates", "rejects unbounded and non-constant loops".
- [x] U10 Blocks compiles with the editor's asc options, runs `checkAbi`,
  simulates in wasm-host and hands the module to the Device tab for upload;
  every example workspace (blink, echo, badge LED chase) compiles and passes
  the ABI check —
  `web/test/blocks.build.test.ts`: "example ... compiles and passes checkAbi", "every 500 ms toggle pin toggles GPIO", "on message -> publish echoes text and numbers", "on pin edge runs on the configured edge and timers fire on_irq".
- [x] U11 Board profiles: `esp-base` (GPIO, timers, pin interrupts) and
  `badge` (conagotchi: `set LEDs` on `badge/leds`, `show text` on
  `badge/text` at most 256 bytes, `pet mode` on `badge/pet`, `on button`
  from `badge/buttons`; no GPIO or timers). Unsupported blocks are disabled.
  Badge payloads get 16 KiB of 1-byte-page memory by rewriting asc's memory
  section, because asc has no page-size option —
  `web/test/blocks.build.test.ts`: "the badge profile publishes on badge/leds from a byte-paged module", "rewrites the memory section to min = max = 16384 bytes with page size 2^0", "detects memory.size and memory.grow, which assume 64 KiB pages", "show text cuts badge text to 256 bytes on a character boundary"; `web/test/blocks.generator.test.ts`: "supports GPIO and timers on esp-base and the badge blocks only on the badge", "shows unsupported blocks disabled in the toolbox", "skips disabled blocks and reports blocks the profile does not support". Not yet run on badge hardware.
- [x] U12 The Blocks workspace persists in localStorage and survives storage
  errors; it exports and imports as JSON —
  `web/test/blocks.storage.test.ts`: "saves and restores the workspace with its profile", "survives storage that throws, is missing or holds garbage", "round-trips the export format and imports bare Blockly workspaces", "rejects files that are not workspaces".
- [x] U13 The browser signs payload commits with Ed25519 (WebCrypto) over
  `"mw-payload-v1\0" || len u32 LE || version u32 LE || sha256(image)`; the
  commit body is `[version u32 LE][signature 64]`, the version is wall-clock
  seconds, bumped to `min_version + 1` with a warning when not ahead; the
  `sys/trust/info` and `sys/trust/claim` bodies are built and parsed by pure,
  unit-tested functions; wire code 5 surfaces as a `Denied` error that says
  how to re-key —
  `web/test/trust.test.ts`: "reproduces the message, signature and commit body of the shared vector" (`web/test/fixtures/trust-vector.json`, shared with the device side), "builds the message from its parts", "encodes and splits the commit body", "encodes the claim body as the raw public key", "fingerprints a key as the first 8 bytes of its SHA-256", "parses the three states, with the key present iff claimed", "round-trips through the encoder", "rejects short bodies, unknown states and a key that disagrees with the state", "relates the device key to this browser's key and decides the upload", "uses wall-clock seconds as the version, bumping past min_version when behind".
- [x] U14 Key custody: the signing key is generated in the browser and
  stored in IndexedDB as a non-extractable CryptoKey; the private key leaves
  the browser only as the backup file made at creation (PKCS#8 under
  AES-256-GCM, key from PBKDF2-SHA-256 with ≥ 600 000 iterations, random
  salt and IV recorded in the file, the public key as GCM additional data),
  which restores with the passphrase; storage failures surface as readable
  errors; the Device tab shows both fingerprints (first 8 bytes of SHA-256
  of the public key) —
  `web/test/trust.test.ts`: "backs up a new key and restores it with the passphrase", "rejects the wrong passphrase and a tampered file", "enforces passphrase length, the iteration floor and the file format", "keeps the non-extractable key, its public key and the backup", "reports unavailable or failing IndexedDB as a KeyStorageError". The Trust panel and its dialogs are not covered by automated tests.
- [x] U15 Uploads read `sys/trust/info` first: signed when the device pins
  this browser's key, unsigned after a confirmation when it is unclaimed,
  unsigned with a note when the firmware answers NotFound (predates trust),
  and refused before sending anything when the device pins another key;
  claim is offered when unclaimed or in the re-key window —
  `web/test/trustUpload.test.ts` against a fake device modelling the trust contract in TypeScript in front of the Rust loader: "uploads unsigned to an unclaimed device after asking", "sends nothing when the unsigned upload is declined", "uploads unsigned to firmware without sys/trust and says why", "claims an unclaimed device and denies a second claim", "signs the commit for a device that pins this key, and the device accepts it", "a commit signed by another key is Denied and the old image stays active", "a replayed version is Denied; a clock behind the device bumps past min_version", "re-keys only while the window opened by holding BOOT is open".
- [x] U16 (0.6) The flasher reads both manifest formats: the ESP Web Tools
  shape (`builds[].chipFamily`, exactly one `parts` entry, plus `wasm` and
  `bsp`) and the older `{version, images}` map, to the same images —
  `web/test/manifest.test.ts`: "U16: parses the ESP Web Tools manifest to the same images, with the BSP", "parses the wasm runtime flag, defaulting to true when absent".

- [x] U16 First run: a Start tab, opened when no tab is saved, says what the
  site does and what a payload is, runs the Blink example in the Blocks
  simulator in one click (asking before it replaces different blocks), and
  gives per-board steps: ESP32, ESP32-S3, ESP32-C3 and ESP8266 flash from
  the Flash tab; Pico and Pico 2 take the rp-base UF2 in BOOTSEL mode;
  protocol-only boards (ESP32-C3, ESP8266, Pico) are marked as storing
  payloads without running them, matching the manifest's `wasm` flags —
  `web/test/onboarding.test.ts`: "covers every bundled ESP image with the manifest's runtime flag", "installs the Picos from UF2, with only the Pico 2 running payloads", "gives every board install, connect and payload steps that point at real tabs", "sends ESP boards to the Flash tab and Picos to BOOTSEL and the UF2", "says protocol-only boards store payloads without running them", "opens Start without a valid saved tab and the saved tab otherwise", "tries an example that exists", "asks before replacing blocks that differ from the example, not otherwise". The click-to-blink path was checked in headless Chrome (about 1.5 s on localhost); the page itself has no DOM test.
- [x] U17 Nothing replaces the payload silently: the Examples menu and
  Import JSON, like the Start tab's "Try it", load at once only when the
  payload has no blocks and no pins or generates the same code, and
  otherwise ask first; cancelling keeps everything and returns the menu to
  its placeholder. A file that is not a payload is reported and changes
  nothing, and an unrelated JSON object, which parses as an empty
  workspace, also asks. Examples add the pins they name by label and never
  change or remove a pin the user chose —
  `web/test/blocks.replace.test.ts`: "asks before any example replaces a different one, never for itself", "adds the pins an example needs from the board, keeping the user's", "reports a file that is not a payload and loads nothing", "asks before an unrelated JSON object, which parses as an empty workspace, wipes work", "loads straight away over an empty payload", "asks before an import replaces different blocks or pins", "re-importing an export of the same payload does not ask", "a bare workspace keeps the current board and pins; a legacy export brings its profile". The menu's confirm, cancel and replace path was driven in headless Chrome; there is no DOM test.
- [ ] U18 The site works on a 390 px phone: no tab scrolls sideways, the
  Examples menu fits its row, the generated-code line numbers stay level
  with their lines, and the Blockly toolbox is slimmer below 600 px and can
  be hidden ("Hide categories"); the Blocks build output is announced to
  screen readers (`aria-live`). Verified by headless Chrome measurements
  and screenshots only (every tab at 390×844 and 1280×800 has a page
  `scrollWidth` no wider than the viewport; gutter and line tops match
  once the editor has been on screen), not by a test, so it stays
  unchecked.
- [x] U19 Pico users install rp-base without a toolchain: the firmware
  manifest may list `uf2.rp2040` and `uf2.rp2350` (file, SHA-256, size,
  version, UF2 family id, factory flag, `wasm` flag), validated strictly:
  only those two chips, family `rp2040` for the Pico and, for the Pico 2,
  only the factory image (partition table plus slot A, family `absolute`;
  the `rp2350-arm-s` update image is for `mwctl fw update` and is
  refused), lower-case 64-digit SHA-256, a size in whole 512-byte blocks.
  `firmware/build-images.sh rp2040 rp2350` builds the UF2s with
  `firmware-rp/build.sh`, copies them next to the ESP images and writes
  those entries; like the ESP images they are build output, not
  committed, and the committed manifest has none. For a listed chip the
  Start tab's install step offers "Download UF2" with the file name, size,
  version and SHA-256, then the BOOTSEL and drag-onto-drive steps (RPI-RP2,
  RP2350); otherwise it says how to build the file. The Pico 2 step says
  the factory image writes the A/B partition table, so it is for the first
  install, and that later updates go over USB with `mwctl fw update`. The
  page saves a download only after checking its UF2 magic, size, family and
  SHA-256 against the manifest, and says this catches a corrupted or
  outdated file and is not a signature —
  `web/test/manifest.test.ts`: "parses what firmware/build-images.sh rp2040 rp2350 writes" (`web/test/fixtures/manifest-uf2.json`, that script's output), "treats an absent uf2 section as none, as in the committed manifest", "rejects an unknown chip and a non-object section", "rejects a wrong family, including an entry filed under the other chip", "lists only the RP2350 factory image, not the mwctl update image", "rejects a missing or malformed sha256, size, version, wasm flag or file name"; `web/test/uf2.test.ts`: "accepts the file the manifest names, with WebCrypto's SHA-256", "refuses a truncated or padded download", "refuses a file whose SHA-256 differs from the manifest", "refuses something that is not a UF2, such as the index.html a server answers for a missing file", "refuses a UF2 of another family, such as the RP2350 update image", "formats sizes for people"; `web/test/onboarding.test.ts`: "sends ESP boards to the Flash tab and, without a UF2 in the manifest, Picos to the build hint", "offers the manifest's UF2 for each Pico with file name, size and version", "says what the download check proves and what it does not", "follows the download with BOOTSEL onto the board's drive, then drag the file, in order", "tells Pico 2 users the factory image sets up A/B and later updates use mwctl fw update", "falls back to the build hint for a chip the manifest has no UF2 for", "marks commands and file names as code spans".
- [ ] U20 The Start tab's "Download UF2" saves a file that installs by
  BOOTSEL drag-and-drop: on a Pico it boots rp-base, and on a Pico 2
  without a partition table it writes the A/B table and boots slot A
  (`mwctl fw info`: running A, slot A bought). Only partly verified, so
  unchecked: the button's fetch-check-save path has no DOM test (in
  headless Chrome against `vite preview` at 1280×800 and 390×844 both
  files saved with the manifest's SHA-256, a wrong SHA-256 and a missing
  file were refused without saving, and the Start tab kept a page
  `scrollWidth` no wider than the viewport); `picotool info -a` on the
  factory UF2 shows the partition table; no file was dragged onto real
  hardware.
- [x] U21 One payload model (IR) behind every editor view: a versioned
  JSON document (`web/src/model/payload.ts`, format `middleware-payload`,
  version 1) holding the board, the pin table (rows: label, pin, function,
  bus, pull, initial level, note; one setting per bus in use), the logic
  sources (version 1: exactly one Blockly workspace) and the state
  machines (empty in version 1). Its shape check is strict: an unknown
  field, a wrong type, a pin outside 0..63, an unknown board, function or
  logic kind, non-empty machines or a newer version is an error naming the
  place. The Blocks tab stores, exports and imports the model; a legacy
  Blocks export or save and a bare Blockly workspace migrate to it with
  the same workspace, the board their profile implies and no pins, so the
  generated code is byte-for-byte unchanged (the legacy save is left in
  place) — `web/test/model.test.ts`: "round-trips through JSON unchanged", "rejects unknown shapes instead of dropping them", "migrates a legacy Blocks export and a bare workspace losslessly", "maps every target to a profile and a pin-rule chip"; `web/test/blocks.storage.test.ts`: "saves and restores the model with its board and pins", "survives storage that throws, is missing or holds garbage", "migrates every legacy save losslessly: same workspace, same profile, same generated code".
- [x] U22 The pin rules are the firmwares' own: the `board-pins` crate
  (no_std, no dependencies) holds the pin maps of the Pico / Pico 2,
  ESP32 (ESP32-CAM), ESP32-S3, ESP32-C3 and ESP8266 (existing, reserved
  with a reason, input-only, ADC, board-wired I2C buses, PWM outputs, SPI
  routing), the RP2040 / RP2350 F1 / F3 function tables and the SPI pin
  resolution; esp-base and rp-base build their `Profile`s on it with
  unchanged results, and protocol-wasm exposes it to the page as
  `boardPins(chip)` and `checkPins(chip, rows, buses)`, which also applies
  the ABI's `i2c_open` / `spi_open` argument rules. The page restates no
  pin rule — `board-pins` unit tests (`cargo test -p board-pins`): "check_follows_exists_reserved_input_only", "every_held_pin_has_a_reason", "adc_pwm_and_i2c_maps", "board_wired_pins_are_payload_pins", "rp_pins_resolve_onto_one_block", "esp_pins_need_only_the_direction", "function_tables_match_the_datasheets"; the unchanged firmware tests (`firmware-rp` "v8_pins_follow_the_pico_board_map", "v8_adc_reads_gpio_26_to_28", "v16_i2c_buses_map_to_the_pico_default_pins", "v15_spi_pins_resolve_onto_one_block_on_both_chips", new "gpio_count_matches_the_pin_map"; `firmware` chip tests); `web/test/pins.test.ts`: "a complete Pico 2 table is clean; the board map comes from Rust".
- [x] U23 The pin table is checked live, per row and per table, in plain
  language with one-click fixes where obvious: missing, malformed or
  duplicate labels (case ignored), duplicate pins, a pin the chip lacks, a
  reserved pin (with what the board uses it for), an output on an
  input-only pin, an ADC or PWM row off the board's analog or PWM pins, an
  I2C row off its bus's board-wired pin or on an unwired bus, an SPI pin
  without the signal or on the other controller than SCK, an incomplete
  I2C pair or SPI bus, and bus clocks or modes outside the ABI's ranges;
  any problem stops Compile before code is built —
  `web/protocol-wasm/src/pins.rs` tests (`cargo test -p protocol-wasm pins`): "a_valid_pico_table_has_no_issues", "labels_must_be_present_valid_and_unique", "duplicate_pins_flag_both_rows", "reserved_input_only_and_missing_pins_say_why", "adc_and_pwm_follow_the_board_map", "i2c_pins_are_fixed_by_the_board_and_come_in_pairs", "spi_pins_must_have_the_signal_and_share_a_controller", "spi_bus_must_be_complete_and_configured", "unknown_functions_are_reported"; `web/test/pins.test.ts`: "reports duplicate pins and labels, reserved and incapable pins in plain language", "checks SPI controllers and incomplete buses, with fixes", "checks the bus settings against the ABI ranges"; `web/test/blocks.pins.test.ts`: "a table with problems stops the build before compiling".
- [x] U24 Boards seed and re-check the table: each board's default table
  (LED: GPIO 25 on the Picos, the flash LED GPIO 4 on the ESP32-CAM, a
  named free header pin elsewhere) passes its own pin rules; picking a
  board on the Start tab or in Blocks replaces a table that is empty or
  still the old board's defaults and otherwise keeps it and re-checks it
  against the new board, with fixes — `web/test/onboarding.test.ts`: "names the Blink pin per board, and every board's default pin passes its pin rules (Rust checkPins)"; `web/test/pins.test.ts`: "switching boards re-checks the same table, with the board's fixes", "seeds a board's defaults only over an empty or untouched table", "keeps one setting per bus in use", "renames labels in pin blocks and lists the labels used".
- [x] U25 The pin table generates code and blocks name pins by label: one
  `const PIN_<label>: i32` per row with a pin and, in `init`, `gpio_output`
  plus the initial level, `gpio_input` with its pull (left to an edge
  handler that configures the pin), `i2c_open(bus, hz)` per complete I2C
  bus and `spi_open(0, sck, mosi, miso, cs, khz, mode)`; `write`, `toggle`,
  `read` and `on ... edge` blocks name a label and require a row of the
  right function; the numeric-pin blocks still work and an empty table
  changes nothing; the table plus the Blink example compiles with asc and
  blinks the label's pin in the simulator — `web/test/blocks.pins.test.ts`: "emits the pin constants and the init setup from the table", "label blocks use the constants and check the row's function", "an empty table changes nothing (numeric pin blocks keep working)", "blinks the LED label on GPIO 25 (pico2)", "blinks the LED label on GPIO 4 (esp32)"; `web/test/blocks.generator.test.ts`: "blink (esp-base) matches test/fixtures/blocks/blink.as and compiles"; `web/test/blocks.build.test.ts`: `example "blink" compiles and passes checkAbi (esp-base)`.
- [ ] U26 The Pins view is a keyboard-editable table that fits a phone:
  a `<table>` with column headers and a labelled control in every cell,
  Tab across, Ctrl+arrow keys between cells, Up / Down between rows in
  text cells, focus and caret kept while typing, problems shown under
  their row and linked with `aria-describedby`, invalid controls marked
  `aria-invalid`, and the table scrolling inside its container so no page
  scrolls sideways at 390 px. Verified in headless Chrome only (at
  1280×900 and 390×844: page `scrollWidth` equal to the viewport with the
  table scrolling 356 of 1014 px on the phone; Ctrl+Right, Ctrl+Down and
  Down moved focus as described; every control labelled; typing kept the
  caret), not by a DOM test, so it stays unchecked.
- [x] U27 State machines are part of the payload model: version 2 adds
  `machines` (at most 4), each with an identifier, a free-text name, an
  initial state, states (name, note, up to 4 entry actions), events (name,
  note) and transition rows (state, event, an optional guard, next state,
  up to 4 actions). Guards are a closed i32 grammar of depth at most 4
  (digital pin read by label, constant, comparison, and / or / not) and
  actions a closed set (write or toggle a pin by label, publish constant
  text, log, raise an event of the same machine). The shape check is
  strict like version 1's, and a version-1 file parses to the same model
  as version 2 with no machines and byte-for-byte the same generated
  code — `web/test/model.test.ts`: "round-trips state machines and copies them", "migrates a version-1 model to version 2 losslessly (no machines, same code)", "rejects unknown shapes instead of dropping them"; `web/test/machines.model.test.ts`: "renaming a state or event renames every reference", "removing a state or event drops what depends on it", "the simple form round-trips through Expr within the depth limit", "lists and renames state and event references".
- [x] U28 State machine tables are checked in Rust by a no_std, no-heap
  crate (`state-table`, also built for `thumbv6m-none-eabi`) through
  protocol-wasm `checkMachines`, in plain language with one-click fixes:
  identifiers and case-insensitive uniqueness of machine ids, state and
  event names; the initial state, every row's state, event and next state
  existing; at most one unguarded row per state and event (an always-true
  guard counts as one); guards in one cell that can both be true, with a
  pin assignment that shows it (decided exactly by trying every 0/1
  combination of up to 12 pins); guard pins that exist and are digital;
  write and toggle on GPIO outputs; publish keys by the middleware key
  rule and not under `sys/`; raise naming an event of the machine; at most
  16 states, 16 events, 64 rows and 4 actions. Unreachable states, events
  no row handles and never-true guards are warnings and do not stop
  Compile; errors do (stage "states") — `cargo test -p state-table`: "a_valid_machine_has_no_issues", "machine_ids_must_be_identifiers_unique_ignoring_case", "state_and_event_names_must_be_identifiers_unique_ignoring_case", "initial_must_name_a_state_with_a_fix_to_the_first", "rows_must_name_known_states_and_events_with_add_fixes", "a_cell_has_at_most_one_unguarded_row", "an_always_true_guard_counts_as_the_unguarded_row", "overlapping_guards_report_a_witness_assignment", "exclusive_guards_in_one_cell_do_not_overlap", "overlap_above_twelve_pins_is_a_warning_not_checked", "guard_pins_must_exist_and_be_digital", "guards_deeper_than_four_or_malformed_are_errors", "write_and_toggle_need_an_output_pin", "publish_keys_follow_the_key_rule_and_avoid_sys", "raise_must_name_an_event_of_the_same_machine", "oversized_machines_report_the_size_and_skip_the_rest", "unreachable_states_are_warnings", "events_no_row_handles_are_dead_warnings", "a_guard_that_is_never_true_is_a_warning"; `cargo test -p protocol-wasm machines`: "cell_messages_name_the_earlier_row", "unknown_state_and_event_messages_offer_to_add_them", "flatten_cuts_deep_expressions_at_one_past_the_limit"; `web/test/machines.check.test.ts`: "a valid machine has no issues", "overlapping guards name the earlier row and a witness assignment", "warns about never-true guards, unreachable states and dead events", "a machine with 17 states reports its size and nothing else"; `web/test/blocks.machines.test.ts`: "stops the build at stage states on a machine error and never on a warning".
- [x] U29 Machines generate bounded i32 code: per machine `SM<k>_<state>`
  and `EV<k>_<event>` constants and a state global; per event one
  function with a single `switch` over the states that handle it, guarded
  rows as `if` / `else if` in table order and the unguarded row as the
  last `else`; on a change of state the new state is set, logged
  (`sm <id>: <from> -> <to> on <event>`) and entered (entry actions, then
  "when <state> is entered" blocks); a self-transition runs only its
  actions; `init` logs and enters each initial state after the pin setup.
  No loops and no allocation; a machine no block names still compiles;
  "raise", "is current?" and "when ... is entered" blocks name
  `<machine>.<event or state>` and an unknown one is an error on the
  block — `web/test/blocks.machines.test.ts`: "emits SM/EV constants per machine, its state and pending globals, and one shared smBusy", "dispatches each event with one switch over the states that have rows for it", "turns guarded rows into if / else if in table order with the unguarded row as the final else, and compiles", "on a change of state sets the state, logs the move and enters the next state (entry actions, then on-enter blocks); a self-transition only runs its actions", "init runs the machines after the pin setup, subscriptions and tick, and before the on-init stacks", "generates byte-identical code without machines", "compiles a machine that no block references (dead but valid)", "reports raise, is-current and on-enter blocks naming an unknown machine, state or event on the block", "reports an invalid table as errors naming the machine and row, leaves the bad parts out and still emits valid code"; `cargo test -p state-table step`: "step_fires_the_first_true_guarded_row_before_the_default", "step_uses_the_first_unguarded_row_and_skips_rows_naming_nothing".
- [x] U30 Raised events are bounded: a raise while any machine is
  stepping (from a table, a block or a flow) is queued, at most one per machine (a second is dropped and
  logged), the queue is drained once in machine order after the outermost
  step, and a raise during the drain is dropped and logged, so one
  external raise runs at most 1 + (number of machines) steps —
  `cargo test -p state-table step`: "raise_queue_holds_one_pending_event_per_machine", "dispatch_while_stepping_queues_instead_of_running", "dispatch_drains_pending_raises_once_in_machine_order", "dispatch_runs_at_most_one_plus_machines_steps"; `web/test/blocks.machines.test.ts`: "raises through the bounded queue: dropped while draining, one slot while stepping, drained once in machine order", "drops a second raise in one step and a raise chained from the drain, logs each, and stays bounded without a trap".
- [x] U31 The "Button toggles LED mode" example (Off / On / Blink on a
  BUTTON press, TICK every 250 ms toggling the LED in Blink) loads from
  the Examples menu with the replace-confirm, sets its machine and adds
  LED and BUTTON from the board's suggestions (BUTTON: a pulled-up input
  on the board's BOOT button or, on the Picos, GPIO 15), and runs in the
  simulator — `web/test/blocks.machines.test.ts`: "runs the mode example on pico2 (LED GPIO 25, BUTTON GPIO 15): PRESS steps Off -> On -> Blink -> Off and TICK toggles the LED in Blink", "runs the mode example on esp32 (LED GPIO 4, BUTTON GPIO 0): PRESS steps Off -> On -> Blink -> Off and TICK toggles the LED in Blink", "needs LED (output) and BUTTON (input) and raises both of its events"; `web/test/blocks.replace.test.ts`: "loading the mode example sets its machines and adds LED and BUTTON from the board's suggestions", "treats machines as work: a payload with machines but no blocks or pins asks before being replaced", "compares the generated code including the machines"; `web/test/onboarding.test.ts`: "suggests a BUTTON pin per board (input, pull-up) that passes its pin rules with the LED (Rust checkPins), outside the default table"; `web/test/blocks.build.test.ts`: `example "mode" compiles and passes checkAbi (esp-base)`.
- [ ] U32 The States view (a sub-view beside Blocks and Pins) edits
  machines with the Pins view's keyboard grid (shared `ui/grid.ts`): a
  transition list by default (state, event, structured guard of up to
  three "pin op value" terms joined by all / any, next state, up to four
  structured actions) and an optional read-only state × event matrix
  whose cells jump to their row or add one; problems under their row with
  `aria-describedby`, errors and warnings styled apart, fixes as buttons;
  renaming a state, event or machine renames the blocks that name it;
  while simulating, the current state is highlighted in the tables and
  named in a live line in the simulator panel. The view's decisions are
  tested (`web/test/states.view.test.ts`: "renames the state, its rows, the initial state and the blocks when the name is valid and free", "does not merge references when the typed name collides with another state", "lists each cell's rows with next state and guard marker, and labels holes", "counts only errors on the tab, since warnings do not block compiling", "says the state and the transition that led to it"), but the page itself was verified in headless Chrome only (1280×900 and 390×844: page `scrollWidth` equal to the viewport, the mode example loaded after the confirm, a duplicate unguarded row blocked Compile at stage states and switched to the view, and two BUTTON presses in the simulator showed "State: Mode = Blink (from On on PRESS)" with the Blink row highlighted), not by a DOM test, so it stays unchecked.

- [x] U33 Flows are part of the payload model: version 3 makes `logic`
  the Blocks workspace followed by at most 8 flows, each an identifier, a
  free-text name, nodes (id, kind, canvas position 0..4000, properties)
  and wires (output port to input port). Node kinds are a closed set
  reusing the machines' types: entry (on start, every N ms, on a message,
  on a pin edge by label, when `<machine>.<state>` is entered), action
  (exactly an `ActionRef`), branch (an `Expr`), repeat (a count) and
  sequence (2 to 4 steps). Version 3 also lets an `ActionRef` raise name
  its machine and adds a state-test leaf to `Expr`. The shape check is
  strict, and version-1 and version-2 files parse to version 3 with no
  flows and byte-for-byte the same generated code — `web/test/model.test.ts`: "rejects unknown shapes instead of dropping them", "round-trips flows, keeps them across workspace edits and copies them", "migrates a version-2 model to version 3 losslessly (no flows, same code)", "migrates a version-1 model to version 3 losslessly (no machines, same code)"; `web/test/flows.model.test.ts`: "names each kind's ports in run order; a sequence has 2 to 4 steps", "connect keeps one wire per port and per input, and refuses self, entry and loop wires", "removing a node drops its wires; changing its kind drops wires from ports it lost; moves clamp", "renames pins, machines, states and events in every reference".
- [x] U34 Flows are checked in Rust by the no_std `state-table` crate's
  `flow` module (no heap, no recursion, built for `thumbv6m-none-eabi`)
  through protocol-wasm `checkFlows`, with the tables' issue and fix
  shape: at most 8 flows, 64 nodes and 128 wires; flow ids unique
  identifiers and node ids unique identifiers; exactly one entry; every
  period, key, pin, machine and state an entry names valid and of the
  right pin function (an edge needs a GPIO input; fix: add it); actions
  and branch conditions by the tables' own rules, a raise naming an
  existing machine and event (fix: add the event to that machine);
  repeat 1..10000, sequence 2..4; wires naming existing nodes, real
  output ports and a non-entry input, each output port and each input
  wired at most once; no cycles (a repeat's body loops by itself), an
  error on each wire of one; nodes the entry does not reach, empty
  repeats and empty branches are warnings and do not stop Compile; errors
  do (stage "flows") — `cargo test -p state-table flow`: "the_mode_example_as_flows_has_no_issues", "every_node_kind_and_port_wired_into_a_tree_is_clean", "a_flow_has_exactly_one_entry", "flow_and_node_ids_are_identifiers_and_unique", "sizes_are_bounded_and_an_oversized_flow_is_not_checked_further", "wires_need_known_nodes_real_ports_and_no_entry_target", "each_output_port_and_each_input_is_wired_at_most_once", "a_cycle_is_an_error_on_each_of_its_wires_even_through_a_repeat_body", "nodes_the_entry_does_not_lead_to_and_empty_repeats_and_branches_are_warnings", "entries_check_their_period_key_pin_and_state", "actions_and_conditions_follow_the_table_rules_and_raises_name_their_machine", "repeat_counts_and_sequence_steps_are_bounded"; `cargo test -p protocol-wasm flows`: "the_mode_example_flows_are_clean", "graph_messages_name_the_other_wire_and_say_what_to_do", "entry_and_action_messages_use_the_tables_wording_with_fixes", "a_flow_without_an_entry_and_a_loop_are_errors"; `web/test/flows.model.test.ts`: "finds no issue in the mode example's flows", "reports a missing entry, a merge, an unknown event with an add fix and an unreachable node, by index", "checks branch conditions' state tests and pins like table guards"; `web/test/flows.gen.test.ts`: "stops the build at stage flows on a flow error and never on a warning".
- [x] U35 Machine tables accept the version-3 additions: a guard's state
  test ("machine M is in state S") and a raise naming another machine's
  event, through the same bounded queue (one external raise from any
  source still runs at most 1 + machines steps). Overlap stays exact: a
  state test is a 0/1 leaf and assignments with one machine in two states
  are skipped — `cargo test -p state-table`: "state_tests_of_one_machine_exclude_each_other_in_the_overlap_check", "guard_state_tests_must_name_a_machine_and_its_state", "a_raise_naming_another_machine_needs_that_machine_and_event", "state_leaves_read_through_the_reader_and_exclude_each_other"; the U30 bound tests unchanged.
- [x] U36 One statement IR behind every logic source: Blocks, table rows
  and entries, and flows all build `web/src/blocks/stmt.ts` statements
  (lines, if / else if / else on i32 conditions, constant-count for) and
  print them with one iterative printer, sharing the lowering of the
  model's `Expr` and `ActionRef`; the Blocks output is byte-for-byte what
  it was (verified for this change on every workspace the test suite
  generates, and kept by the fixture tests) — `web/test/flows.gen.test.ts`: "prints nested if / else if / else and for loops with two spaces per level, iteratively", "lowers the model's expressions and actions, state tests and raises through the host", "generates byte-identical Blocks code when there are no flows"; `web/test/blocks.generator.test.ts` fixture tests ("blink (esp-base) matches test/fixtures/blocks/blink.as and compiles" and the other fixtures); `web/test/blocks.machines.test.ts`: "generates byte-identical code without machines".
- [x] U37 Flows generate bounded code: the tree under the entry lowers to
  straight-line statements (a branch to `if`, a repeat to a constant
  `for`, a sequence to its steps in order) placed where the matching hat
  block's body goes (init, the shared tick, the message handler, the pin's
  interrupt, the state's enter function); each node outside a repeat body
  logs `flow <id>: <node>`; a bad flow is reported and left out and the
  output still compiles — `web/test/flows.gen.test.ts`: "lowers branch, repeat and sequence onto the shared if / for, logging each node outside repeat bodies", "places each entry's body where its hat block's would go", "reports bad flows, leaves the bad parts out and still emits valid code"; `web/test/blocks.build.test.ts`: `example "mode-flows" compiles and passes checkAbi (esp-base)`; `web/test/blocks.generator.test.ts`: "mode-flows (esp-base) matches test/fixtures/blocks/mode-flows.as and compiles".
- [x] U38 The "Button toggles LED mode" example drawn as flows (a BUTTON
  edge raising Mode.PRESS, every 250 ms raising Mode.TICK, entering
  Mode.Blink logging) produces the same simulator trace as the Blocks
  version: every log line, publish, pin setup, pin write and interrupt,
  and the LED level after each step, apart from the flows' own `flow`
  lines — `web/test/flows.gen.test.ts`: "produces the same simulator trace as the Blocks version on pico2", "produces the same simulator trace as the Blocks version on esp32".
- [ ] U39 The Flows view (a sub-view beside Blocks, Pins and States)
  edits flows on a hand-written SVG canvas (drag nodes, drag a wire from
  an output port onto a node, select and delete nodes and wires, pan by
  dragging the background, zoom buttons, arrow keys move the focused
  node) and in an outline of the same flow in run order (each port a menu
  of nodes it may lead to or a new node, Earlier / Later, Delete), with a
  palette, Undo for the last 50 changes and a properties grid (grid.ts)
  for the selected node; problems mark their node or wire and list with
  fixes; while simulating, each flow's latest run is highlighted and named
  in a line in the simulator panel; on a phone the outline is the default
  and the canvas scrolls inside its box. The view's decisions are tested
  (`web/test/flows.view.test.ts`: "places the input on top and outputs along the bottom in run order", "says what each node does", "makes new entries, actions and conditions from the pins and machines, keeping what applies", "offers as targets only nodes that keep the flow a tree, and reorders a chain", "keeps at most fifty undo steps", "maps issues to nodes, counts errors on the tab, and turns fixes into changes", "names the node each flow last ran in the simulator line"; `web/test/flows.model.test.ts`: "lists the outline in run order with each port's chain under its node, then nodes nothing leads to", "parses flow log lines and keeps each flow's latest run"), but the page itself was verified in headless Chrome only (1280×900 and 390×844: page `scrollWidth` equal to the viewport, the canvas box 356 px wide scrolling 576 px on the phone, a real mouse drag moved a node and rewired a port, a duplicate flow name, a 5 ms period and an unreachable node were reported and blocked Compile at stage flows, two Undos restored them, and two BUTTON presses highlighted the Press flow's nodes), not by a DOM test, so it stays unchecked.

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

SPI (ABI 2) and I2C (ABI 3) per base firmware:

| board | SPI | I2C |
|---|---|---|
| ESP32 (ESP32-CAM, `firmware/`) | bus 0: SPI2 with DMA, payload-chosen pins | bus 0: SDA 26 / SCL 27 (the camera's SCCB), up to 400 kHz |
| ESP32-S3 (`firmware/`) | bus 0: SPI2 with GDMA, payload-chosen pins | bus 0: SDA 5 / SCL 6 (a `Profile` constant), up to 400 kHz (E24) |
| ESP32-C3 (`firmware/`) | no payloads run (no `wasm` runtime: wasmi needs CAS atomics) | same |
| ESP8266 (`firmware-esp8266/`) | no payloads run (too little DRAM for wasmi) | same |
| Pico 2, RP2350 (`firmware-rp/`) | bus 0 on SPI0 or SPI1, pins checked against the function table, FIFO pumped (V15, V17) | buses 0 (GP4/GP5) and 1 (GP6/GP7), up to 1 MHz (V16) |
| Pico, RP2040 (`firmware-rp/`) | no payloads run (no CAS atomics); the bus drivers are built and the pure core host tested | same |

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
  `< 4`, frequency 1 Hz to 40 MHz (1 MHz until R15 raised it for camera
  clocks), duty a `u16` where 65535 is always on) —
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

#### PWM and ADC on the ESP base firmware (0.5)

The ESP32 and ESP32-S3 base firmware implement `pwm_open`, `pwm_set`,
`pwm_stop` and `adc_read` with esp-hal 1.2's `ledc` and `analog::adc`
drivers (both behind esp-hal's `unstable` feature). The ESP32-C3 (no
runtime) and the ESP8266 are unchanged. The decisions are pure and host
tested in `esp_base` (`chip`, `periph::pwm`, `periph::claims`); the
hardware code is `firmware/src/shell/pwm.rs` and `adc.rs`. The pin tables
below are a contract with payload generators (Skygraph), served by
`sys/board`. The ESP32 row changed once, when the ESP32 build became the
AI-Thinker ESP32-CAM's (R15): 0.5 had GPIO 4, 13, 18, 19, 23, 25, 26, 27,
which on that board are camera data, sync and SCCB lines; (0,0) is now the
camera's XCLK (GPIO 0, a strapping pin) and timers 2–3 are `Unsupported`.

| ABI (t,c) | (0,0) | (0,1) | (1,0) | (1,1) | (2,0) | (2,1) | (3,0) | (3,1) |
|---|---|---|---|---|---|---|---|---|
| ESP32 (ESP32-CAM) GPIO | 0 | 4 | 13 | 14 | — | — | — | — |
| ESP32-S3 GPIO | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 |

| chip | `adc_read` pins (ADC1, 11 dB, raw 12-bit) |
|---|---|
| ESP32 | GPIO32–39 |
| ESP32-S3 | GPIO1–10 |

- [x] X9 ABI PWM timers 0–3 are LEDC low-speed timers 0–3 and channel `c`
  of timer `t` is LEDC low-speed channel `2·t + c`, on the fixed pin in the
  table above; timers 4–5, channels 2–3 and a timer the board maps no pin
  to (ESP32 timers 2–3) are `Unsupported`. The pins avoid strapping (except
  the ESP32-CAM's XCLK, GPIO 0), flash/PSRAM, USB, UART0, ADC1 and the
  default I2C pins —
  `firmware` `periph::pwm::tests::x9_abi_timers_and_channels_map_to_ledc_low_speed_channels`,
  `x9_pwm_pins_avoid_strapping_flash_usb_console_adc1_and_i2c`;
  `chip::tests::profiles_match_chips` (`Profile::pwm_pins`, `pwm_pin`).
- [x] X10 A timer gets the widest duty resolution its 80 MHz APB divider
  allows (`div = (clk << 8) / f / 2^bits ∈ 256..=0x3FFFF`, 8.8 fixed point),
  capped at 16 bits on ESP32 and 14 on ESP32-S3; ESP32 falls back to the
  1 MHz REF_TICK when the APB divider overflows (as esp-hal does), and
  ESP32-S3 refuses 1–4 Hz with `Unsupported`. Duty is scaled as
  `raw = (d·2^bits + 32767) / 65535`, so 0 is always low and 65535 always
  high — `periph::pwm::tests::x10_resolution_is_the_widest_the_apb_divider_allows`,
  `x10_low_frequencies_use_ref_tick_on_esp32_and_are_refused_on_esp32s3`,
  `x10_duty_scales_with_rounding_to_the_full_range`.
- [x] X11 `pwm_open` configures the timer and binds both its channels at
  duty 0; reopening retunes the timer and rescales the duties it keeps.
  `pwm_set` on a closed timer is `Busy`; a channel's first `pwm_set` claims
  and routes its pin (`PinBank::route_out(pin, LEDC_LS_SIGn)`, the esp-hal
  channel built with `NoPin` from short-lived handles over the stored
  `LEDC`). `pwm_stop` zeroes both channels, drives their routed pins low as
  GPIO outputs and frees them; stopping a closed timer is a no-op. Payload
  stop (`EspDevice` drop) zeroes all eight channels before the pins are
  released — `periph::pwm::tests::x11_open_binds_both_channels_at_zero_and_reopen_rescales_kept_duties`,
  `x11_set_needs_an_open_timer_and_unmapped_ids_are_unsupported`,
  `x11_stop_returns_routed_pins_and_closes_the_timer`; `shell/device.rs`
  (esp32/esp32s3 builds).
- [x] X12 `adc_read` is an ADC1 one-shot conversion at 11 dB attenuation
  returning the raw, uncalibrated 12-bit value, on ESP32 GPIO32–39 and
  ESP32-S3 GPIO1–10; ADC2 is never used (it conflicts with Wi-Fi). A pin that
  does not exist is `InvalidArgument`, a pin without an ADC1 channel
  `Unsupported`, and a conversion that does not finish within 1 ms
  `Transport`. ADC1 is built at boot with every channel enabled, before the
  pin bank; a read steals the pin's `GPIOn` only inside `adc_read`, with the
  bank slot in `Mode::Analog`, and payload stop rebuilds the slot's `Flex`,
  which takes the pad back from the analog (RTC) mux —
  `chip::tests::x12_adc1_pins_per_chip`;
  `claims::tests::x12_adc_claims_only_adc1_pins`; `shell/adc.rs`,
  `shell/gpio.rs` (esp32/esp32s3 builds).
- [x] X13 PWM and ADC pins are exclusive claims (`Owner::Pwm`,
  `Owner::Adc`): neither can be taken by SPI, UART, a GPIO interrupt or the
  other, a pin held elsewhere is `InvalidArgument`, and plain `gpio_*` calls
  on them are refused until `pwm_stop` (PWM) or payload stop (ADC) —
  `claims::tests::x13_pwm_and_adc_claims_are_exclusive`.
- [ ] X14 On hardware (ESP32 and ESP32-S3): each of the eight PWM pins
  shows the requested frequency and duty on a scope (including 1 Hz on
  ESP32 via REF_TICK, 1 MHz at 6 bits, duty 0 and 65535), reopening retunes
  without a stuck output, `pwm_stop` and payload stop leave the pins low;
  `adc_read` reads about 0 at GND and 4095 near 3.3 V on every ADC1 pin, and
  an ADC pin works as plain GPIO again after the payload stops. Builds
  (`firmware/build.sh esp32`, `esp32s3`; `esp32c3` unchanged); not yet run
  on a board.
- [x] X15 The browser simulator implements the ABI 3 PWM and ADC imports
  like the desktop simulator (X7): PWM records each open timer's frequency
  and channel duties (reopen keeps them, set on a closed timer is `Busy`)
  and reports `pwmOpen`/`pwmSet`/`pwmStop` events; `adc_read` returns the
  value set with `setAdc(pin, value)` (0–4095, 0 until set); `pwm()` lists
  the open timers. It accepts every ABI timer, channel and pin, not just the
  ESP32 tables — protocol-wasm `simulator::periph::tests::x15_pwm_records_duty_per_channel_and_reopen_keeps_it`,
  `x15_adc_values_are_settable_and_bounded`,
  `simulator::tests::x15_device_records_pwm_events_and_reads_set_adc_values`,
  `x15_payload_reads_adc_drives_pwm_and_stop_makes_set_busy` (fixture
  `web/test/fixtures/pwm_adc.wat`); `web/test/abi3.test.ts`.

### Signed payloads (trust)

From 0.4 a device can pin an Ed25519 public key (the root of trust, held by
the browser or by `mwctl`) on first use; from then on it installs a payload
only when the commit is signed by that key with a strictly increasing
version. The rules and wire types live in `middleware::loader::trust`; the
SHA-256 and Ed25519 checks sit behind the `trust` cargo feature (no_std, no
heap: `sha2` 0.10.9 with `compress`, `ed25519-dalek` 2.2.0 verify only, no
default features). The ESP base firmware (`firmware/`) enables it for every
chip; the ESP8266 firmware does not.

System keys (all integers little-endian; the full table, including the 0.3
keys, is in the `loader` module docs):

| key                 | request body                                  | response body |
|---------------------|-----------------------------------------------|---------------|
| `sys/trust/info`    | empty                                         | `[state u8][min_version u32][key 32, iff state != 0]`; state 0 Unclaimed, 1 Claimed, 2 Claimed with the re-key window open |
| `sys/trust/claim`   | `[public key 32]`                             | empty; `Denied` when claimed and the window is closed |
| `sys/loader/commit` | empty (unsigned) or `[version u32][signature 64]` | empty; `Denied` when the trust rules refuse it |

The signature is Ed25519 (RFC 8032) over the 54-byte message
`b"mw-payload-v1\0" || len u32 || version u32 || sha256(image)`; the device
hashes the image while the in-order chunks arrive. `HandlerError::Denied`
(wire code 5, gRPC `PERMISSION_DENIED`) is new in 0.4.

**Threat model.** The trust check protects the payload loader: once a key is
pinned, nobody without that key can install, replace or roll back a payload
over any transport, and an unsigned, badly signed or replayed upload leaves
the previous payload running. It does not protect the firmware itself:
without eFuse secure boot and flash encryption, anyone with the USB port can
reflash the base firmware (or the `trust` partition) through the ROM
bootloader, and anyone who can press the BOOT button can open the re-key
window and pin their own key — physical presence is the authority for
re-keying by design. Rollback and replay are prevented by
`version > min_version` (strictly), with `min_version` persisted before the
new image can boot, so a captured signed commit can be used at most once
and never after a newer one. Before the first claim the device trusts on
first use: it accepts any commit, signed or not, exactly as 0.3 did, and the
first claimant pins the key — claim a device before exposing it.

- [x] T1 The signed message, signed commit body, claim body and
  `sys/trust/info` encode and parse as specified; wrong lengths are
  `InvalidRequest` — `loader::trust::tests::signed_message_matches_golden_vector`,
  `commit_bodies_parse_and_encode`, `claim_bodies_must_be_32_bytes`,
  `trust_info_round_trips_every_state`; `loader::tests::commands_round_trip_through_parse`,
  `parse_rejects_bad_bodies_and_passes_app_keys`.
- [x] T2 The loader hashes the image (SHA-256) incrementally as chunks arrive
  and hands the digest and the commit's signature to the shell; the hash
  matches RustCrypto `Sha256` at every length and split —
  `loader::sha256::tests::known_answers`, `matches_sha2_at_every_length_and_split`,
  `equal_inputs_give_equal_states`; `loader::tests::commit_passes_digest_and_signature_through`.
- [x] T3 The golden vector `web/test/fixtures/trust-vector.json` reproduces
  in Rust: message, signature verification, commit body —
  `loader::trust::tests::crypto::golden_vector_verifies`; mwctl
  `sign::tests::golden_vector_commit_body_is_reproduced`,
  `golden_vector_file_matches_the_constants`; protocol-wasm
  `loader::tests::wire_helpers_match_the_golden_vector`,
  `signed_upload_of_the_golden_vector_installs_and_replay_is_denied`.
- [x] T4 A claim is accepted when Unclaimed or when the re-key window is
  open, stores the key and resets `min_version` to 0; otherwise `Denied`; a
  key that is not a curve point is `InvalidRequest` —
  `loader::trust::tests::crypto::claim_transitions`; firmware
  `dispatch::tests::trust::claim_on_unclaimed_pins_key_and_info_reports_it`,
  `reclaim_needs_the_rekey_window`.
- [x] T5 When Claimed, a commit is accepted only if signed by the pinned key
  over this image with `version > min_version`, and then raises
  `min_version`; unsigned, other key, altered version, other image or
  length, corrupt or non-canonical signature, replay and rollback are all
  `Denied` and the installed image stays —
  `loader::trust::tests::crypto::claimed_commit_denials`; firmware
  `dispatch::tests::trust::signed_commit_installs_and_raises_min_version`,
  `denied_commits_keep_the_installed_image`; mwctl
  `device::tests::unsigned_upload_to_a_claimed_device_is_denied`,
  `replayed_or_older_version_is_denied_and_keeps_the_image`. The trust
  store refuses to save a record that lowers `min_version` for the same
  key (`StoreError::Rollback`), and when an activation fails after its
  trust record was saved the shell adopts the record on flash, so RAM
  never trusts an older `min_version` than a reboot would —
  `trust_store::tests::saves_never_lower_min_version_for_the_same_key`.
- [x] T6 When Unclaimed every commit is accepted and any signature ignored
  (TOFU) — `loader::trust::tests::crypto::unclaimed_accepts_any_commit`;
  firmware `dispatch::tests::trust::unclaimed_device_accepts_unsigned_and_signed_commits`;
  mwctl `device::tests::unclaimed_device_accepts_signed_and_unsigned_commits`.
- [x] T7 Presence: the BOOT button held ≥ 3 s, pressed after boot (a pin
  held or stuck low from boot never fires; an unreadable pin counts as
  released), opens the re-key window for 60 s. The pin is read with the
  pull-up enabled; when a payload owns GPIO0 it counts only as an input
  with pull-up, so a payload that floats, pulls down, drives or reroutes
  the pin cannot fake a press —
  firmware `presence::tests::three_second_hold_after_release_fires_once`,
  `short_press_does_not_fire_and_release_resets`,
  `held_from_boot_never_fires_until_released`,
  `unreadable_pin_counts_as_released`, `second_press_fires_again`;
  `loader::trust::tests::window_opens_for_sixty_seconds`.
- [x] T8 The trust record (magic, format version, sequence, CRC-32) is
  written to the other of two sectors, so power loss at any flash operation
  leaves the old or the new record, never none —
  firmware `trust_store::tests::records_round_trip_and_reject_damage`,
  `blank_or_foreign_partition_is_unclaimed`,
  `saves_alternate_sectors_and_survive_reopen`,
  `power_loss_at_any_operation_keeps_old_or_new_record`,
  `newest_valid_record_wins_and_a_corrupt_one_falls_back`.
  Trust record format 2 (`firmware/src/trust_store.rs`, 60 bytes): magic
  `MWTR`, format `2`, state (0 Unclaimed, 1 Claimed), two zero bytes,
  sequence u32, `min_version` u32, public key 32, last accepted control
  counter u64 (O17), CRC-32 of bytes 0..56. Format 1 (52 bytes: the same
  without the counter, CRC-32 of bytes 0..48) still loads, with counter 0;
  the next save writes format 2 into the other sector, so the migration
  has the power-loss safety of any save and is never undone —
  `trust_store::tests::format_1_records_load_with_counter_zero_and_migrate_on_the_next_save`,
  `unknown_formats_and_an_unclaimed_record_with_a_counter_are_refused`,
  `saves_never_lower_the_control_counter_for_the_same_key`.
- [x] T9 A raised `min_version` reaches flash after the new image's CRC
  check and before its slot header, so the new image never boots with the
  old `min_version`; a failed save writes no header —
  firmware `store::tests::activate_with_runs_after_crc_and_before_header`.
- [x] T10 `HandlerError::Denied` is wire code 5 and round trips through the
  one-byte code and gRPC status —
  `transport::reqrep::tests::handler_error_codes_round_trip`,
  `grpc::status::tests::handler_errors_round_trip_through_codes`,
  `codes_map_back_to_handler_errors`.
- [x] T11 `trust` and `trust,serial` build for `thumbv6m-none-eabi` without
  std or cloud dependencies, and mwctl still enables no middleware feature —
  `scripts/check-features.sh`.
- [x] T12 `mwctl key new|pub`, `trust info|claim --key` and
  `upload --key [--version]` (default version: Unix seconds) —
  mwctl `cli::tests::parses_trust_key_and_signed_upload_commands`,
  `rejects_bad_trust_and_key_usage`; `plan::tests::signed_plan_ends_in_a_68_byte_commit_the_loader_passes_on`;
  `sign::tests::key_files_must_be_32_bytes`, `default_version_is_unix_seconds`;
  `device::tests::trust_info_round_trips_through_claim`,
  `second_claim_is_denied_with_an_explanation`,
  `claim_then_signed_upload_installs_and_raises_min_version`;
  `report::tests::trust_info_shows_state_version_and_key`.
- [x] T13 The browser simulator runs the real trust rules (claim, presence,
  signed commit) — protocol-wasm
  `loader::tests::unclaimed_accepts_unsigned_and_rekey_needs_presence`,
  `signed_upload_of_the_golden_vector_installs_and_replay_is_denied`.
- [ ] T14 On hardware (ESP32-S3): claim, signed upload, refusal of an
  unsigned and of a replayed upload with the previous payload still
  running, the trust record surviving reboot, and BOOT-held re-keying.
  Builds (`firmware/build.sh esp32s3`, `esp32`, `esp32c3`); not yet run on
  a board.

### Over-the-air payload update (O…)

A claimed ESP base firmware device (ESP32, ESP32-S3: feature `espnow`)
accepts signed payload updates over ESP-NOW. The host reaches it through a
USB↔ESP-NOW bridge (`tools/espnow-bridge/esp8266`, an ESP8266 running a
dumb-radio Arduino sketch) with `mwctl --port <bridge> --espnow <MAC>`.
Every decision is Rust: the request/response rules in `middleware::rpc`
(always built, `no_std`, no heap), the over-the-air policy in
`esp_base::ota`, and the host client in `mwctl::air`.

**Request/response frames.** A request or response is one ESP-NOW frame,
magic `MR` (not the publications' `MW`, so publication receivers skip it as
foreign), version 1: `MR | 1 | kind (1 request, 2 response) | id u16 LE |
to MAC 6 | key length u8 + key + body` (requests) or `| status u8 + body`
(responses; status 0 or a `HandlerError` code). Key and body share 237
bytes, so a `sys/loader/chunk` over the air carries 217 image bytes. The
requester resends on timeout with the **same** id; the responder replays a
cached response to a repeat instead of running the handler again, so a
resent `commit` or `start` never acts twice. Responses go unicast to the
requester.

**Policy** (`esp_base::ota::admit`, before dispatch, over the air only;
from 2026-10-30, after a security review found that anyone in radio range
could stop, restart or abort the running payload — ESP-NOW is
unauthenticated and its source MAC spoofable — and that a non-owner's
unsigned commit aborted the owner's transfer):

| request over the air        | unclaimed | claimed |
|-----------------------------|-----------|---------|
| `sys/info`, `sys/trust/info` | `Denied` | allowed (read only) |
| `sys/loader/begin`          | `Denied`  | allowed: *stages* into the inactive slot; the active image and the running payload are untouched; takes the transfer over |
| `sys/loader/chunk`          | `Denied`  | the transfer's owner only (else `InvalidRequest`) |
| `sys/loader/commit`         | `Denied`  | owner checked first (a non-owner gets `InvalidRequest`, loader untouched); then signed only, `TrustState::accept_commit` (pinned key, version above the last); the owner's unsigned commit is `Denied` and ends its transfer; an accepted commit activates the staged image and stops the old payload |
| `sys/payload/start`, `stop` | `Denied`  | signed control request only (below), else `Denied` |
| `sys/trust/claim`, anything else | `Denied` | `Denied` |

**Publications and requests** (with middleware 0.5, E18): two frame
formats cross the radio and they never mix. Publications (`MW`) are
`app/...` from a payload and `disc/...` from the radio task's own beacon
only, received on `app/...` and `disc/...` only; `sys/...` is never a
publication over the radio. Requests (`MR`) are the only way `sys/...`
crosses it, they are built only by the radio task (responses) or the host
(requests), and the table above admits them. A payload cannot send an
`MR` frame: whatever it publishes is wrapped in an `MW` sample.

Serial is unchanged (physical access is the authority there): a serial
`begin` stops the payload and shows `Loading`, `start`/`stop` take empty
bodies, unsigned commits follow the trust rules. `sys/info` stays refused
over the air on an unclaimed device: an unclaimed device has no key to
check anything against and does not advertise itself to the air.

**Staging.** The running payload never reads flash after its start (the
image buffer is only needed to compile it) and a transfer only writes the
inactive slot, so a `begin` over the air (`Context::staging`) erases and
fills the inactive slot while the payload keeps running; the status stays
what it was during the transfer and when it fails. Only a commit that the
trust rules accept writes the slot header, which activates the new image,
and stops the old payload (`Action::Activate` stops it; status
`Stopped`). A storage failure stops the payload only after a serial
`begin` (`dispatch::after_storage_failure`). The worst a radio attacker
without the key can do is a denial of service of the *update*: take the
transfer over with a `begin`, or spoof the owner's MAC and send chunks
that spoil the staged image, whose commit then fails its CRC or signature
check. It can never stop, start, replace or roll back the running
payload.

**Signed control requests** (`middleware::loader::trust`): over the air
`sys/payload/start` and `stop` carry `[counter u64 LE][signature 64]` (72
bytes), an Ed25519 signature by the pinned key over the 29-byte message
`b"mw-control-v1\0" || op u8 (1 start, 2 stop) || target MAC 6 ||
counter u64 LE`, the target being the device's own station MAC.
`TrustState::accept_control` accepts it only when the signature verifies
(`verify_strict`) and the counter is strictly above the last accepted one;
the controller saves the new counter with the trust record *before*
acting (failure: `Internal`, nothing done) and then dispatches the key
with an empty body. A captured request can therefore not be replayed
(counter used), sent to another device (MAC signed), turned into the
other command (op signed) or used as a payload signature (different
tag). A claim of a new key resets the counter to 0. `mwctl` uses the Unix
time in milliseconds as the counter (`--counter` overrides), so no read
is needed and `sys/trust/info` is unchanged; a host whose clock lags the
last accepted counter is refused with an explanation. `stop` stays
available over the air (signed), because stopping a payload remotely is
the point of the feature and the signature makes it as safe as `start`;
`upload --start` over the air sends the commit and then a signed start.

**One loader.** Serial and radio share the dispatcher's loader. The
requester whose `begin` started the transfer owns it; a `begin` from anyone
takes it over, and a `chunk` or `commit` from a non-owner is
`InvalidRequest` without touching the loader — checked before the
signature rule, so a non-owner's unsigned commit cannot end the owner's
transfer. Both serial ports count as one requester; radio requesters are
told apart by MAC.

**Bridge framing** (`mwctl::bridge`, 460800 baud): COBS + CRC-16 frames,
`Send {dst, data}` host → bridge, `Recv {src, data}`, `Hello {mac,
channel, version}`, `Stats` (six counters) and `Sent {ok, dst}` (the
driver's send callback: MAC-level acknowledgement) bridge → host, `Query`
host → bridge.

- [x] O1 Request and response frames have the layout above, round trip,
  fill exactly one 250-byte frame at the limits (64-byte key with 173 body
  bytes; 217-byte chunk; 237-byte response body), refuse larger messages
  before sending, and reject malformed frames —
  `rpc::tests::o1_frames_round_trip_and_layout`,
  `o1_largest_messages_fill_one_frame_and_larger_ones_are_refused`,
  `o1_malformed_frames_are_rejected`.
- [x] O2 A requester accepts only the response with its call's id, from the
  peer it asked, addressed to itself, and its attempts are bounded —
  `rpc::tests::o2_a_call_accepts_only_its_own_response_and_bounds_attempts`.
- [x] O3 A responder runs each request once: repeats are replayed from a
  bounded least-recently-used cache, repeats (and other requests) while one
  runs are dropped, an abandoned or 10 s old request frees it, and bodies
  over the cache size are not cached —
  `rpc::tests::o3_a_request_runs_once_and_repeats_replay_its_response`,
  `o3_the_cache_is_bounded_least_recently_used_and_skips_large_bodies`,
  `o3_an_abandoned_or_stale_request_frees_the_responder`.
- [x] O4 `espnow::rpc::RpcRequester` (`Requester`) and `RpcResponder`
  (`Responder`) work end to end over an in-memory air that loses,
  duplicates and reorders frames: a lost request is resent with the same
  id, a lost response is replayed without running the handler again, a
  duplicated or delayed request runs once, a delayed response never
  answers a later call, attempts are bounded, and frames for or from other
  stations are ignored —
  `espnow::rpc::tests::o4_request_and_response_end_to_end`,
  `o4_a_lost_request_is_resent_with_the_same_id`,
  `o4_a_lost_response_is_replayed_without_running_the_handler_again`,
  `o4_a_duplicated_request_runs_once`, `o4_reordered_frames_never_mix_answers`,
  `o4_attempts_are_bounded_and_other_stations_are_ignored`,
  `o4_oversized_requests_and_small_buffers_are_errors`. On the 0.5
  contract `RpcRequester::request_with` bounds a call with
  `options.timeout` (`Retry::within`: shorter and fewer attempts, at least
  one; `None` keeps `attempts` × `timeout_ms`), refuses a `Target::Node`
  other than its peer (`WrongTarget`) before sending, and the responder's
  handler sees the requester's MAC as `RequestMeta::sender` —
  `o4_request_options_bound_the_call_and_target_only_the_peer`; `espnow` builds for
  `thumbv6m-none-eabi` (`scripts/check-features.sh`).
- [x] O5 The bridge framing round trips, rejects bad CRC, COBS, types and
  lengths, and is found behind the ESP8266's delimiter-less boot banner;
  the C++ codec of the sketch reproduces the Rust golden frames byte for
  byte (also checked by an independent Python COBS/CRC script) —
  mwctl `bridge::tests::o5_messages_round_trip_through_the_wire`,
  `o5_golden_frames_match_the_bridge_firmware`, `o5_bad_frames_are_rejected`,
  `o5_frames_split_out_of_a_noisy_stream`, `o5_macs_parse_and_format`;
  `tools/espnow-bridge/check.sh` (`test_bridge_frame.cpp`, then the sketch
  compiles for `esp8266:esp8266:generic`).
- [x] O6 Over the air an unclaimed device refuses everything; a claimed one
  allows only the update keys, never `sys/trust/claim`, and only signed
  commits, while serial keeps its rules —
  `ota::tests::o6_an_unclaimed_device_refuses_the_air_entirely`,
  `o6_a_claimed_device_allows_only_the_update_keys_over_the_air` (also
  `app/...` and `disc/...` keys: they are publications, never requests),
  `o6_a_commit_over_the_air_must_be_signed`.
- [x] O7 The begin's requester owns the transfer; a begin from the other
  side takes it over and the abandoned side's chunks and commit are
  refused without disturbing it; the owner's unsigned commit over the air
  ends the transfer; a signed replay is `Denied` with the image kept
  (through the real dispatcher) —
  `ota::tests::o7_only_the_owner_continues_a_transfer`,
  `o7_begin_takes_the_transfer_and_its_end_frees_it`,
  `ota::tests::session::o7_a_serial_begin_aborts_an_air_transfer`,
  `o7_an_air_begin_aborts_a_serial_transfer_and_completes_signed`.
- [x] O8 The radio task keeps at most four requesters registered as
  unicast peers (least recently used replaced), and its request counters
  saturate and are logged on change at most every 5 s —
  `radio::tests::o8_unicast_peers_are_bounded_and_rpc_counters_saturate`.
  The shell (`src/shell/radio.rs`) taps `MR` frames off the radio before
  the transport, hands new requests to the controller as `Port::Radio`
  requests tagged with the sender's MAC, and answers repeats from the
  cache.
- [x] O9 `mwctl --espnow <MAC>` runs `info`, `trust info|claim`, `upload`
  (217-byte chunks, signed commit) and `start`/`stop` through the bridge
  with the same `Device` code as serial; it learns the bridge's MAC from
  `Hello`, retransmits with the same id (300 ms per attempt, 5 s for
  begin/commit/start, 10 attempts, a 30 ms wait after an unacknowledged
  send), and never runs a step twice on a lossy link; `monitor` and
  `reset` are refused with `--espnow` —
  mwctl `air::tests::o9_info_over_the_air_learns_the_bridge_first`,
  `o9_upload_over_the_air_survives_loss_and_never_runs_a_step_twice`,
  `o9_refusals_are_device_errors_and_attempts_are_bounded`;
  `plan::tests::o9_air_plans_use_chunks_that_fit_one_espnow_frame`;
  `cli::tests::o9_espnow_selects_the_bridge_and_refuses_local_commands`.
- [x] O10 On hardware (2026-10-30; ESP32-S3 `e4:b0:63:81:d3:24` on
  `/dev/ttyACM0`, bridge ESP8266EX `5c:cf:7f:72:f6:54` on `/dev/ttyUSB0`):
  after `trust claim` over serial, `info` and `trust info` over the air
  answered; a signed `upload echo.wasm --start` (version 1000) installed
  and started, and the S3's own serial showed `ota: sys/loader/begin from
  5c:cf:7f:72:f6:54`, `trust: signed payload accepted, min_version 1000`,
  `echo: started`, status running; `mwctl --port /dev/ttyACM0 info` said
  running.
- [x] O11 On the same hardware, over the air: with the device unclaimed,
  `info`, `trust info`, `upload` and `stop` were `Denied` (`ota: ...
  refused: device unclaimed`); claimed, an unsigned upload was `Denied` at
  commit (`commit over the air must be signed`, transfer ended, status
  stopped), a stale (1000) and an older (999) signed version were `Denied`
  (`needs a valid signature and version > 1000`), and `trust claim` was
  `Denied` (`not allowed over the air`).
- [x] O12 On the same hardware: a serial upload begun 0.6 s into an
  over-the-air upload took over (the air side failed at step 35/109 with
  `InvalidRequest`, the device logged `chunk from 5c:cf:... refused:
  another requester owns the transfer`) and installed; an over-the-air
  upload begun 0.25 s into a serial upload took over the same way (serial
  failed at step 23/48) and installed and started.
- [x] O13 Throughput on the same hardware: `bench.wasm` (23072 bytes, 109
  requests) installed signed in 1.62 s, 1.62 s and 1.69 s (about 14 KB/s),
  `echo.wasm` (2686 bytes) with start in 0.32 s; no retransmission and no
  unacknowledged send in any hardware run (the retransmission path is
  covered by O4 and O9 on the host).
- [x] O14 A transfer begun over the air stages into the inactive slot:
  `begin` is `Erase { stop: false }`, the status (`Running` included) is
  unchanged by begin, chunks, failures, a spoiled image or a refused
  commit, and only an accepted commit activates the image and stops the
  old payload; the active image stays loadable throughout; a storage
  failure stops the payload only after a serial `begin`; a serial `begin`
  still stops it and shows `Loading` —
  firmware `dispatch::tests::o14_a_staged_transfer_leaves_the_running_payload_alone_until_commit`,
  `o14_a_failed_or_spoiled_staged_transfer_keeps_the_payload_running`,
  `o14_storage_failures_stop_the_payload_only_after_a_serial_begin`;
  `store::tests::o14_staging_keeps_the_active_image_loadable_and_a_spoiled_one_never_activates`;
  `ota::tests::session::o14_an_air_transfer_never_disturbs_the_running_payload_before_a_signed_commit`.
- [x] O15 Ownership is checked before the signature rule: a non-owner's
  unsigned commit is `InvalidRequest` (`NotOwner`) and leaves the owner's
  transfer to complete; only the owner's unsigned commit ends it —
  `ota::tests::o15_ownership_is_checked_before_the_signature_rule`,
  `ota::tests::session::o15_a_non_owners_unsigned_commit_leaves_the_owners_transfer_to_complete`.
- [x] O16 Over the air `start`/`stop` need a signed control request: empty
  or malformed bodies are `Denied` (`UnsignedControl`), a signature by
  another key, for another device or the other command, or with a counter
  not above the last is `Denied` (`ControlRefused`); an accepted one acts
  once and its replay is `Denied`; serial needs no signature —
  `ota::tests::o16_start_and_stop_over_the_air_need_a_signed_body`,
  `o16_a_signed_control_names_this_device_this_command_and_a_new_counter`,
  `ota::tests::session::o16_signed_start_and_stop_act_once_and_replays_are_denied`;
  middleware `loader::trust::tests::control_message_and_bodies_have_the_documented_layout`,
  `crypto::control_golden_vector_verifies`,
  `crypto::control_requests_need_the_pinned_key_this_device_this_op_and_a_new_counter`;
  firmware `dispatch::tests::trust::reclaim_needs_the_rekey_window` (a
  claim resets the counter, a commit keeps it).
- [x] O17 The last accepted control counter is persisted with the trust
  record (format 2, migrated from format 1, see T8) before the request
  acts, and is never lowered for the same key —
  `trust_store::tests::saves_never_lower_the_control_counter_for_the_same_key`,
  `format_1_records_load_with_counter_zero_and_migrate_on_the_next_save`,
  `power_loss_at_any_operation_keeps_old_or_new_record`.
- [x] O18 `mwctl --espnow` signs `start`, `stop` and `upload --start` with
  `--key` for the target MAC (counter `--counter` or Unix milliseconds),
  refuses `upload`/`start`/`stop` over the air without `--key` and
  `start`/`stop --key` over serial with a usage error, and explains a
  `Denied` start/stop; the control signature reproduces the middleware
  golden vector —
  mwctl `sign::tests::golden_control_vector_is_reproduced`,
  `default_counter_is_unix_milliseconds`;
  `cli::tests::o9_espnow_selects_the_bridge_and_refuses_local_commands`,
  `rejects_bad_trust_and_key_usage`; `air::tests::o9_upload_over_the_air_survives_loss_and_never_runs_a_step_twice`,
  `o9_refusals_are_device_errors_and_attempts_are_bounded`;
  `error::tests` (`sys/payload/stop` explanation).
- [x] O19 On hardware (2026-10-30, same boards; firmware flashed over the
  format-1 trust record claimed at `min_version` 3000 with `echo`
  running, which loaded as claimed/3000/counter 0): over the air, unsigned
  `stop` and `start`, a 72-byte zero "signature", and `trust claim` were
  `Denied` (`refused: start/stop over the air must be signed by the pinned
  key`, `control signature invalid ...`, `not allowed over the air`);
  `begin` and 20 chunks of `bench.wasm` without a commit left `echo`
  running (`ota: staging into the inactive slot; the payload keeps
  running`); a signed `stop` stopped it, the identical replay, a signed
  `start` with the same counter and one with an older counter were
  `Denied`, a signed `start` started it. A signed `upload blink.wasm
  --start` (3001) logged `new image active; stopping the payload that ran
  the old one`, then the signed start ran blink; a signed upload of
  `bench.wasm` (3002, 23 077 bytes, 1.85 s) staged while blink kept
  publishing `app/blink/state` until the commit. With a serial upload in
  progress, eight unsigned air commits got `InvalidRequest` (`another
  requester owns the transfer`) and the serial upload installed; with an
  air upload in progress, 49 serial unsigned commits were refused the same
  way and the air upload installed. A signed upload with a corrupted chunk
  failed its commit (`InvalidRequest`) with `bench` still running and
  `min_version` unchanged. The trust partition read back as two format-2
  records (seq 17 and 18, `min_version` 3005, counters 1793329532721 and
  1793329644857); after a reset the replay of counter 1793329644857 was
  `Denied`. Serial `stop`/`start` worked unsigned. After the full gate
  the gated build was flashed again and the refusals, staging, signed
  stop/start, replay and a signed `upload echo.wasm --start` (3006)
  repeated with the same results.

### A/B firmware OTA (RP2350)

The RP base firmware on a Raspberry Pi Pico 2 (RP2350) replaces its own
firmware over the same USB CDC serial port and framing that carries
payloads, driven by `mwctl fw`. The RP2350 bootrom does the dangerous part:
two partitions of the partition table are linked as an A/B pair, the
bootrom boots the valid image with the higher version, and an image
flagged try-before-you-buy (TBYB) is booted only by a flash-update reboot,
on trial, until the firmware calls the bootrom's explicit buy within the
watchdog window; otherwise the next boot skips it and the previous image
runs. The references are the RP2350 datasheet's bootrom chapter (§5.1
"Bootrom concepts", §5.4 "Bootrom APIs", §5.9 "Metadata block details");
the PDF was not available while this was written, so every encoding was
taken from the pico-sdk headers that the datasheet tabulates
(`boot/picobin.h`, `boot/bootrom_constants.h`, `pico/bootrom.h`,
`embedded_start_block.inc.S`), and the behaviours that could not be
checked are listed below as assumptions to verify on hardware.

The Pico 1 (RP2040) bootrom has no partitions, versions or TBYB, so it is
out of scope by design: `mwctl` refuses an RP2040 UF2 and the RP2040
firmware does not answer `sys/fw/*`.

Everything decidable on the host lives in `middleware::fw_ota` (always
built, `no_std`, no heap): the `sys/fw/*` wire format (`wire`), the pure
state machine `FwUpdate` (`machine`), the IMAGE_DEF parser (`picobin`), the
persisted record (`record`) and the hardware seam `FwPlatform`
(`platform`). The shell `FwService` (`service`, feature `trust`) runs a
request against a platform. `fw_ota::sim` (feature `fw-sim`, host only) is
an in-memory model of the RP2350 bootrom and flash that implements
`FwPlatform`, so the whole lifecycle, power cuts included, is tested on
the host; mwctl's fake device runs the real `FwService` on it.

**Protocol.** Seven keys, all integers little-endian (the byte layouts are
in the `fw_ota` module docs and `wire::FwInfo`):

| key              | request body                         | response |
|------------------|--------------------------------------|----------|
| `sys/fw/info`    | empty                                | state, running slot, each slot's version and TBYB flag, the anti-rollback floor, node id, the update in progress (36 bytes) |
| `sys/fw/begin`   | image length, version, SHA-256 (40 bytes) | the target slot (1 byte), always the inactive one |
| `sys/fw/chunk`   | offset, 1 to 512 bytes               | empty |
| `sys/fw/commit`  | empty, or an Ed25519 signature (64 bytes) | empty |
| `sys/fw/reboot`  | empty                                | empty; flash-update reboot about 100 ms later |
| `sys/fw/confirm` | empty                                | empty; explicit buy |
| `sys/fw/abort`   | empty                                | empty |

The version is the image's IMAGE_DEF `major << 16 | minor`; it must exceed
both the persisted `min_version` and the running image's version, and the
device installs an image only as the version its own IMAGE_DEF declares,
so the bootrom's A/B choice and the anti-rollback floor always agree.
Chunks are 512 bytes at multiples of 512 (the last one shorter), in any
order; a chunk already received is acknowledged and not written again; a
misaligned, overlapping or out-of-range chunk is refused. The commit
signature uses the payload signing key pinned by `sys/trust/claim` and
covers the 64-byte message `"mw-firmware-v1\0"`, length, version, target
slot, the device's 8-byte node id and the image's SHA-256, so a signature
is good for one image, one version, one slot of one device and never for a
payload or a control request (different tags). An unclaimed device
accepts any commit (trust on first use, as for payloads). Every request is
idempotent on resend, and a refused request leaves the state unchanged,
except a commit whose image fails verification, which abandons the
transfer.

**State machine.** Idle goes to Receiving on `begin` (a repeated
identical `begin` resumes, any other restarts). When every chunk has
arrived, `commit` checks the version and the signature, then the device
hashes what it holds and reads the IMAGE_DEF: the hash must match and the
image must be an RP2350 executable flagged TBYB, without an OTP rollback
version, of the declared version, with a block of its metadata loop in
the first sector's last page (the sentinel, below). The state is then
Verified; the device persists `min_version` and the pending update, writes
the image's first sector, and is Committed. `reboot` (only when Committed)
asks the bootrom for a flash-update reboot into the new slot. That boot is
a Trial (the bootrom reports a buy pending); `confirm` (only on trial, and
`Unavailable` until the firmware's own self-test passed) buys the image
and the state is Confirmed. A trial that ends without a buy (watchdog,
reset, power cut, or `abort`, which reboots normally) comes back in the
old slot as RolledBack. `abort` also abandons a transfer or a committed
update (erasing the new slot's first sector). Verified is transient: it
is where a commit stays when installing failed, and another commit
retries.

**Where the image goes.** Flash is erased in 4 KiB sectors, each just
before its first chunk is written (computed from the received-chunk
bitmap alone). The first sector, which holds the image's first metadata
block, is erased at `begin` and its chunks are kept in RAM until a commit
has passed every check and the record is saved; only then is it written.
So the inactive slot never holds a bootable image that was not verified
and authorized, and the running slot is never erased or written (the
shell refuses any effect on it before touching flash).

**Sentinel block.** The bootrom's explicit buy erases the image's first
sector and programs it again with the TBYB bit clear, page by page. A
power cut part way would leave the first pages (an IMAGE_DEF that is no
longer TBYB) and an erased tail: with the whole block loop in those first
pages the bootrom would boot a torn image. The power-loss test found this.
An updatable image therefore carries a second, empty block in the last
256-byte page of its first sector (from offset 0xF00), linked into the
loop, so the loop closes only when the sector is complete again and a
torn buy leaves the slot without a valid image. `picobin::image_def_block`
and `picobin::sentinel_block` build both blocks; the device and `mwctl`
refuse images without the sentinel.

**Power loss.** Every state is recovered from flash and the bootrom at
boot (`FwUpdate::boot`): with nothing pending the state is Idle; with an
update pending and the bootrom reporting a buy pending it is Trial; if the
pending slot is the one running (bought, record not yet settled) it is
Confirmed and the record is settled; if the reboot had been requested and
the old slot runs, it is RolledBack and the record is settled; if the
reboot had not been requested it is Committed when the slot still hashes
to the committed image, else Idle with the slot's first sector erased. A
commit's version is used up once its record is saved, even when the update
does not complete, so a captured signed commit can never be replayed.

**Host.** `mwctl --port <PATH> fw info | fw update <IMAGE> [--key <KEYFILE>]
[--version MAJOR.MINOR] [--no-reboot] | fw reboot | fw confirm | fw abort`.
The image is a UF2 (RP2350 families, reassembled from the flash base; an
RP2040 UF2 is refused) or a raw binary; an ELF is refused with the
`picotool` and `objcopy` commands that convert it. The version comes from
the image's IMAGE_DEF; `--version` is only a cross-check. `fw update`
refuses locally an image the device would refuse, sends every chunk with
progress (a lost reply is resent and deduplicated), signs the commit for
the slot and node id the device reported, reboots, reopens the same port
path for up to 12 s (it never scans), waits for the trial and confirms it,
retrying while the self-test is still running. It waits up to 15 s for
`sys/fw/commit` (the device hashes the whole image) and 5 s for
`sys/fw/begin` and `sys/fw/confirm` (sector erases).

- [x] Y1 The `sys/fw/*` requests and the `sys/fw/info` response encode and
  parse symmetrically; every other body length is `InvalidRequest`, an
  unknown `sys/fw/` key `NotFound`, any other key is not the updater's;
  a 512-byte chunk request fits one serial frame —
  `fw_ota::wire::tests::y1_commands_round_trip_through_parse`,
  `y1_parse_refuses_bad_bodies_without_panicking`, `y1_info_round_trips`;
  `fw_ota::tests::y1_slots_and_sizes`.
- [x] Y2 The IMAGE_DEF parser follows a block loop from the first 4 KiB
  (bounded blocks, bounded loop length), returns the last IMAGE_DEF with
  its flags, version (and rollback version) and the loop's highest block
  in the first sector, and treats any malformed metadata as no image,
  never panicking —
  `fw_ota::picobin::tests::y2_block_built_here_parses_with_version_and_tbyb`,
  `y2_two_block_loop_like_the_pico_sdk`, `y2_rollback_version_is_read`,
  `y2_malformed_metadata_is_no_image`, `y2_random_metadata_never_panics`,
  `y2_sentinel_block_closes_the_loop`, `y2_read_errors_propagate`.
- [x] Y3 The transfer targets the inactive slot, refuses an image larger
  than the slot or not newer than the floor, accepts aligned chunks in any
  order once each, refuses misaligned, overlapping, out-of-range and
  wrong-length chunks without changing state, erases each sector once on
  its first chunk and stashes the first sector, resumes on an identical
  `begin`, and abandons the transfer when verification fails (hash,
  missing or wrong IMAGE_DEF, not TBYB, rollback version, version
  mismatch) — `fw_ota::machine::tests::y3_happy_path_reaches_committed_with_sector_erases_computed`,
  `y3_chunks_are_idempotent_and_bounded`,
  `y3_begin_targets_the_inactive_slot_and_checks_size_and_version`,
  `y3_verification_failures_abandon_the_transfer`,
  `y3_chunk_set_counts_and_sectors`.
- [x] Y4 The signed message is the 64-byte layout above with its own tag;
  a claimed device accepts a commit only with a signature by the pinned
  key over exactly this length, version, slot, node and hash (strict
  verification), an unclaimed one accepts any commit, and a build without
  `trust` refuses every commit of a claimed device —
  `fw_ota::wire::tests::y4_signed_message_layout`,
  `fw_ota::machine::tests::y4_claimed_devices_need_a_signature_binding_slot_node_and_version`
  *(trust)*, `y4_without_crypto_a_claimed_device_refuses_every_commit`
  *(no features)*.
- [x] Y5 `reboot` is accepted only when committed, `confirm` only on trial
  (`Unavailable` before the self-test passed, idempotent once confirmed),
  and `abort` abandons a transfer or committed update, reverts a trial
  and is a no-op otherwise —
  `fw_ota::machine::tests::y5_reboot_confirm_and_abort_follow_the_state`.
- [x] Y6 The boot reconciliation maps every combination of record and
  bootrom state to the table under "Power loss" —
  `fw_ota::machine::tests::y6_boot_reconciles_every_record_with_the_bootrom_state`.
- [x] Y7 On the bootrom model, through `FwService` and the wire format: an
  update installs into B, boots on trial, is confirmed (TBYB cleared) and
  then boots normally, and the next update goes into A; a trial without a
  confirm rolls back and its version cannot be replayed; `abort` reverts a
  trial and discards a committed update; refused requests (early reboot
  or confirm, old version, oversize, unsigned or wrongly bound
  signatures) never touch the running slot and leave no image in the
  other; a bad image or corrupted flash fails the commit with nothing
  persisted; resent requests are idempotent —
  `fw_ota::sim::tests::y7_full_lifecycle_updates_both_slots_in_turn`,
  `y7_trial_without_confirm_rolls_back_and_its_version_is_used_up`,
  `y7_abort_rejects_a_trial_and_abandons_transfers`,
  `y7_refusals_leave_the_running_image_alone`,
  `y7_corrupted_flash_or_a_bad_image_fails_the_commit`,
  `y7_resent_requests_are_idempotent` *(fw-sim)*.
- [x] Y8 A power cut at every mutating operation of a complete update
  (sector erases and writes, the record saves, the reboot request, the
  bootrom's buy and the boot-time settling) leaves a device that boots,
  boots only the factory image or a bought new one, starts the updater,
  and completes the update when the host retries —
  `fw_ota::sim::tests::y8_power_loss_at_every_operation_is_recoverable`,
  `y8_power_loss_while_settling_at_boot` *(fw-sim)*.
- [x] Y9 `mwctl fw` loads UF2 and raw images and refuses ELF, RP2040,
  non-TBYB, sentinel-less and mismatched images with an explanation;
  parses its commands; updates, reboots and confirms a fake Pico 2
  running the real updater on the bootrom model, with progress, signed
  commits, lost replies resent, refusals explained, confirm retried while
  the self-test runs, and abort on trial rolling back; prints `fw info`;
  waits longer for the slow requests —
  `mwctl` `fw::tests::y9_images_load_from_bin_and_uf2_and_are_checked_like_the_device`,
  `y9_versions_parse_as_major_minor`,
  `y9_update_reboot_and_confirm_against_the_bootrom_model`,
  `y9_refusals_are_explained`,
  `y9_lost_replies_are_resent_and_the_device_deduplicates`,
  `y9_abort_on_trial_rolls_back`; `cli::tests::y9_parses_fw_commands`;
  `report::tests::y9_fw_info_names_slots_versions_and_progress`;
  `link::tests::timeouts_extend_begin_only`.
- [x] Y10 The persisted record (60 bytes: magic, format, flags, slot,
  sequence number, `min_version`, the pending update's version, length and
  hash, CRC-32) round trips, every single-bit error and an erased sector
  read as no record, the newer of two copies wins, and a save that would
  lower `min_version` is detected —
  `fw_ota::record::tests::y10_records_round_trip_and_reject_damage`,
  `y10_newest_takes_the_higher_sequence_and_floors_never_drop`.
- [x] Y11 `fw_ota` is always built (its declarations carry no cfg gate);
  the RP2350 sets (no features, `trust`, `trust,serial`,
  `trust,serial,postcard`) build for `thumbv8m.main-none-eabihf` with no
  cloud dependency and no host feature; `fw-sim` builds on the host and
  is a host feature that no embedded set enables; mwctl's shipped build
  uses no middleware feature (the model is a dev-dependency) —
  `scripts/check-features.sh` (Y11 lines).

**RP2350 integration** (`firmware-rp/`, `rp_base::ota`, feature
`fw-update`, on in the `rp2350` build). The RP base firmware serves
`sys/fw/*` with `FwService` on `rp_base::ota::platform::RpPlatform`, an
`FwPlatform` over any NOR flash and a boot ROM seam (`rp_base::ota::rom::Rom`),
built for each request from borrows: the flash is the payload store's, lent
by the request service, and the ROM seam is owned by the backend
(`rp_base::ota::AbUpdate`). `AbUpdate::boot` runs `FwService::boot` once
at startup, before the request service opens the store; if it fails (most
often because the board has no partition table, so the boot ROM booted no
slot) every `sys/fw/*` request is answered `Unavailable` and the boot log
says why. The request service hands every `sys/fw/*` key to it before the
payload dispatcher, with the node id `sys/info` reports, the trust store's
root of trust and the self-test.

No `unsafe` is written and the only boot ROM function called is `reboot`.
embassy-rp 0.10 exposes `get_sys_info` and `explicit_buy` only as
`unsafe fn` (raw buffer pointers); rp235x-hal 0.4 has a safe `boot_info`
wrapper but cannot be linked beside embassy-rp (it forces its PAC's `rt`
feature, and both PACs then define the interrupt vector table
`__INTERRUPTS`). So the integration gets the same facts safely:

- the booted slot is read from the QMI's `ATRANS0` register (the boot ROM
  maps the booted partition at 0x10000000; the register's base is that
  partition's first sector), a safe register read through embassy-rp's PAC
  (feature `unstable-pac`);
- "buy pending" is the running image's own TBYB flag in its IMAGE_DEF in
  flash. The boot ROM launches a TBYB image only on a flash-update boot of
  its partition (otherwise it is an image condition failure), and the buy
  clears the flag, so a running image still flagged TBYB is exactly a trial;
- the explicit buy is done by the firmware: it reads the running image's
  first sector into a 4 KiB stack buffer, clears the TBYB bit of the
  IMAGE_TYPE item, and erases and programs that sector with embassy-rp's
  safe blocking flash inside `cortex_m::interrupt::free`, checks it read
  back, then stops the watchdog (embassy-rp's `Watchdog::stop`). That is
  the only effect of the ROM's `explicit_buy` that applies here: its
  downgrade erase of the other slot never applies (versions only grow) and
  neither does the OTP rollback version (images carry none, and the chip
  is not in secure boot). The image's first sector holds the vector table
  and the two metadata blocks but no code (`.text` starts at 0x1000), so
  nothing the CPU fetches is erased while interrupts are masked, and a
  power cut part way leaves the sentinel's loop open, so the other slot
  boots;
- `reboot_flash_update` is `rom_data::reboot(REBOOT_TYPE_FLASH_UPDATE, 100,
  0x10000000 + partition offset, 0)` (an XIP address, as picotool passes
  after a load) and `reboot_normal` is `REBOOT_TYPE_NORMAL` with the same
  delay; both return, so the reply leaves first.

The platform addresses slots by partition offset through the untranslated
window (see "RP base firmware"), bounds every access by the slot size,
refuses an erase or write of the running slot before touching flash
(defence in depth: the pure core never asks), erases one 4 KiB sector per
call so USB is serviced between sectors, pads a short final write with
`0xFF` and bounces unaligned reads. The record lives in two sectors of
unpartitioned space after slot B (0x2FE000): a save erases the sector not
holding the newest valid record and writes the new one there with the next
sequence number, fields first and magic last. Nothing in the firmware
feeds or starts the watchdog, so a hung trial image is reverted by the
boot ROM's trial watchdog if it arms one, and by any reset otherwise.

The self-test that gates `sys/fw/confirm` is `fw_update::SelfTest`: the
host has configured USB (embassy-usb's `Handler::configured`), the payload
store opened (its flash is readable and lends the flash to the updater)
and the trust record opened (the key that authorizes the next update is
readable). Until it passes, `confirm` is `Unavailable` and `mwctl`
retries.

**Image and partition table.** `build.rs` writes the image's first-sector
metadata into the linker script (`rp2350-image.x`, included by
`memory-rp2350.x`) from `rp_base::ota::image`: the vector table at 0x000,
the IMAGE_DEF block at 0x200 (IMAGE_TYPE flags 0x9021, an RP2350 Arm secure
executable with TBYB; VERSION `major.minor`), the sentinel block at 0xF00
linked into its loop, `.text` from 0x1000; embassy-rp's own IMAGE_DEF is
off (`imagedef-none`). The version is `build.sh rp2350 --version
MAJOR.MINOR` (environment variable `RP_BASE_FW_VERSION`), else the crate's
`major.minor`, and is also what `sys/info` reports. The partition table is
built in Rust from the flash map (`rp_base::ota::table`, rather than a
picotool JSON file, so the table, the linker script and the updater share
one map): one block at flash offset 0 with partition 0 "A" (sectors
0x002 to 0x17F) and partition 1 "B" (0x180 to 0x2FD, link type "A
partition", value 0), both accepting the `rp2350-arm-s` UF2 family, all
permissions everywhere (they only govern the ROM's own flash APIs and
picotool), and unpartitioned space accepting the `absolute` family. The
host tool `rp-image` (`firmware-rp/src/bin/rp-image.rs`, run by
`build.sh rp2350` after picotool has converted the ELF to a UF2) checks the
image as `mwctl fw update` and the device do and writes
`rp-base-rp2350-V-update.bin` and `-update.uf2` (the TBYB image for `mwctl
fw update`) and `rp-base-rp2350-V-factory.uf2` and `-factory.bin`: the
partition table at offset 0, the same image with only its TBYB bit cleared
(bought) in slot A, and erased pages at the start of slot B and of both
record sectors, in the `absolute` family, leaving the payload store and the
trust record as they are. `picotool info -a` on these files shows the
partition table (A at 0x2000 to 0x180000, B at 0x180000 to 0x2FE000, "B w/
0") and the image's two metadata blocks at 0x10000200 (image def, version,
"not bought" on the update image) and 0x10000F00 (ignored).

**Factory flashing** of a Pico 2, for version 1.0 (any version works; the
first update must be higher). First build with `firmware-rp/build.sh
rp2350 --version 1.0`, which leaves `rp-base-rp2350-1.0-factory.uf2` in
`firmware-rp/target/thumbv8m.main-none-eabihf/release/`. Then put the board
in BOOTSEL mode: hold the BOOTSEL button while plugging in USB (or, from
the previous rp-base, any way that reaches the boot ROM's USB mode); the
RP2350 drive and picotool see it. Optionally run `picotool partition info`
to see what table the board holds now (a board running the pre-A/B rp-base
has none). Load the factory UF2 with `picotool load --ignore-partitions -v
rp-base-rp2350-1.0-factory.uf2`, which writes every block at its absolute
address whatever table the board holds and verifies it, and start it with
`picotool reboot`; on a board without a partition table, dragging the
file onto the RP2350 drive does the same. `picotool partition info` (in
BOOTSEL again) then lists partitions A and B. On the first boot the
firmware logs `fw: booted slot A, state idle, min_version 0.0` on
`sys/log`, and `mwctl --port <PATH> fw info` reports state idle, running
A, slot A 1.0 (bought), slot B empty, `min_version` 0.0. The payload store
and the pinned key, if any, are unchanged, so a claimed board still needs
`--key` for signed commits. To start from a clean board instead,
`picotool erase` before the load wipes everything, including the trust
record.

- [x] Y12 `FwPlatform` on the RP2350 as above: the booted slot from the
  translated base (no slot without a partition table, and the updater is
  then `Unavailable` while the payload side works); the trial flag from
  the running image's TBYB flag; slots bounded and the running one never
  erased or written; reads, erases and writes at physical offsets through
  the untranslated window; short writes padded and odd reads bounced; the
  record alternating between its two sectors with every power cut during
  a save leaving the previous record; the buy clearing only the TBYB bit
  in place with interrupts masked, then stopping the watchdog, idempotent,
  and a torn buy leaving the slot unbootable so the other one boots;
  flash-update reboots at the partition's XIP address; the self-test
  (USB configured, store and trust record open) gating confirm; and the
  whole lifecycle through `mwctl` and the request service on a model of
  the boot ROM's A/B choice: factory 1.0 in A, a signed update to 1.1 into
  B, trial, confirm, reboot into B, an update to 1.2 into A left
  unconfirmed and reset (rolled back to B, 1.2 used up), then 1.3 into A —
  `ota::tests::y12_slots_are_bounded_and_the_running_one_is_never_written`,
  `y12_the_record_alternates_sectors_and_survives_power_cuts`,
  `y12_explicit_buy_clears_tbyb_in_place_masked_and_stops_the_watchdog`,
  `y12_a_torn_buy_leaves_slot_b_unbootable_and_a_boots`,
  `y12_without_the_partition_table_the_updater_is_unavailable`,
  `y12_mwctl_updates_confirms_and_rolls_back_a_pico_2`,
  `y12_confirm_waits_for_the_self_test`,
  `y12_requests_reach_the_updater_and_unknown_fw_keys_are_not_found`;
  `ota::rom::tests::y12_the_translated_base_names_the_running_slot`;
  `flash::tests::y12_window_reads_through_the_untranslated_alias_and_writes_physically`;
  `fw_update::tests::y12_the_self_test_needs_usb_the_store_and_the_trust_record`
  (firmware-rp `cargo test --lib`).
- [x] Y13 The image and flash layout: the partition table makes the two
  equal slots an A/B pair (B linked to A, `rp2350-arm-s` accepted), with
  the record sectors outside both; every build's IMAGE_DEF is the 0x9021
  block with the build's `major.minor` at 0x200, linked to the sentinel at
  0xF00, with no code in the first sector; the linker script carries
  exactly those words; versions parse as `MAJOR.MINOR` only; the factory
  flash is the table plus the update image bought (one bit) in slot A and
  boots A; images `mwctl` refuses are refused here too; the UF2s
  reassemble to the images and the factory one erases slot B and the
  record but not the payload store or the trust record; and the image
  `build.sh rp2350` produced installs through `mwctl fw update
  --no-reboot` on the board model, reboots on trial, is confirmed and is
  then byte for byte the factory image of itself —
  `layout::tests::y13_rp2350_has_a_table_two_equal_slots_and_the_record_outside_them`,
  `v2_memory_x_flash_lengths_match_the_maps`;
  `ota::table::tests::y13_partition_table_makes_slots_a_and_b_an_ab_pair`;
  `ota::image::tests::y13_blocks_are_the_picobin_blocks_with_tbyb_and_the_sentinel_in_the_last_page`,
  `y13_linker_script_carries_exactly_the_block_words`,
  `y13_versions_parse_as_major_minor_only`;
  `ota::tests::y13_factory_flash_is_the_table_and_the_update_image_bought_in_slot_a`,
  `y13_images_mwctl_refuses_are_refused_here_too`,
  `y13_uf2s_reassemble_to_the_images_and_the_factory_one_erases_b_and_the_record`,
  `y13_the_built_update_image_installs_through_mwctl` (with
  `RP_BASE_UPDATE_IMAGE` naming the built `-update.bin` or `-update.uf2`;
  skipped without it). `picotool info -a` on the built files shows the
  table and the blocks as described above.

**Unreadable trust record.** The root of trust decides whether a commit
must be signed, so when the trust record cannot be read the firmware does
not treat the device as unclaimed: the request service hands the backend
no root of trust (`FwContext::trust` is `None`) and the backend refuses
`begin`, `chunk` and `commit` with `Unavailable` before the updater sees
them. `info` is read only. `abort` only abandons a transfer (erasing the
inactive slot's first sector) or reverts a trial to the image that booted
before, so it can only move the device back to firmware it already ran.
`reboot` only boots an image already committed, which was authorized
while the record was readable. `confirm` passes this rule for the same
reason, but the self-test requires the trust record open, so a trial that
boots with the record unreadable is never bought and reverts at the next
reset: the firmware that cannot read its root of trust never becomes
permanent.

- [x] Y16 With the trust record unreadable, firmware `begin`, `chunk` and
  `commit` (signed or not) are `Unavailable` and leave the inactive slot
  untouched, `info` and `abort` work, and a trial booted with the record
  unreadable cannot be confirmed (self-test) and rolls back on reset —
  `ota::tests::y16_an_unreadable_trust_record_refuses_firmware_installs`,
  `y16_a_trial_booted_with_the_trust_record_unreadable_is_never_bought`.

**Boot ROM questions.** Settled from the pico-sdk and picotool sources:
the flash-update reboot's `p0` is the XIP address of the partition start
(picotool's load passes `0x10000000 + start`); the A/B partition booted is
the partition the ROM maps at 0x10000000 (`ATRANS0`), which stands in for
`boot_info.partition`; a TBYB image is launched only on a flash-update boot
of its own partition (rp235x-hal's documentation of the boot diagnostic
`IMAGE_CONDITION_FAILURE`), which makes the TBYB flag the "buy pending"
bit; the explicit buy's downgrade erase and OTP rollback never apply to
these images. Open, for the hardware run (Y14): whether the ROM arms a
watchdog for a trial and with which period (16.7 s is from memory), and
whether embassy-rp's clock setup, which reprograms the watchdog's tick
generator to 1 µs from `clk_ref`, changes that period (nothing in the
firmware feeds or starts the watchdog); that the ROM uses the last
IMAGE_DEF of a block loop (these images have one); that the ROM compares
`major.minor` between A and B and boots the higher bought one; what the
ROM does with a flash-update reboot aimed at a slot holding no valid image
(the firmware never asks for one: `reboot` needs a committed, verified
image); whether `ATRANS0`'s size is exactly the partition's (only its base
is used); and that a power cut during the firmware's own buy leaves B
unbootable on silicon as in the model.

- [ ] Y14 The bootrom behaves as the model assumes, to verify on a Pico 2:
  a normal boot skips an unbought TBYB image; a flash-update reboot boots
  it (and maps its partition at 0x10000000); without a buy the watchdog
  reboot (if the ROM arms one; about 16 s, from memory of the datasheet)
  or any reset boots the other slot; the bootrom picks the higher
  `major.minor` between A and B when neither is TBYB; the firmware's buy
  (rewriting the image's first sector with the TBYB bit clear) makes the
  image boot normally afterwards; the last IMAGE_DEF of a block loop is
  the one used; and a power cut during the buy's rewrite leaves the slot
  unbootable thanks to the sentinel.
- [ ] Y15 End to end on a Pico 2 over USB CDC: `mwctl fw update` with a
  signed image installs into the inactive slot, the port comes back after
  the reboot within the reconnect window, the trial is confirmed, a later
  reset boots the new image, a trial left unconfirmed reverts, and power
  cuts during the transfer, after the commit and during the trial leave a
  device that boots and finishes the update on a retry.

### DMA routes (ABI 4)

Frame streams between devices without copying through guest memory. A
payload configures a *route* and the host runs it: a **source route**
captures frames (a camera, or the synthetic test pattern standing in for
one) into host-owned DMA-capable buffers and publishes each frame on a key
as chunks; a **sink route** subscribes a key and DMAs the chunks to an SPI
bus opened with `spi_open`, sending a per-frame preamble (e.g. the
`CASET`/`RASET`/`RAMWR` window, D/C low for command bytes and high for
their parameters) before each frame's first chunk and the pixel data with
D/C high. Pixels are RGB565 big-endian (display-ready), default 120×120
(28 800 bytes). Over ESP-NOW a message carries at most 179 bytes whatever
the key, so a chunk is a 12-byte header and up to 166 data bytes (174
chunks per 120×120 frame). The contract is `wasm_abi::route` (imports,
config layouts) and `stream` (chunk wire format, chunker, sink rules,
test pattern), both pure, `no_std` and heap-free. A source may instead
stream baseline JPEG (the OV2640's native output, decoded by the receiving
payload): each frame is one JPEG of varying length (R14).

- [x] R1 ABI 4 is a superset of ABI 3: `route_source` (5 params),
  `route_sink` (5), `route_stop` (1) and `route_stat` (2) are appended to
  `wasm_abi::IMPORTS`, `ABI_VERSION` is 4, and ABI 1–3 modules still load
  because imports are checked by name and signature only —
  `wasm_abi::tests::revisions_are_prefixes_of_the_import_table`;
  `wasm-host/tests/abi4.rs` `r1_every_import_links_and_older_revisions_still_load`;
  `abi-check` `tests/payloads.rs` `abi3_examples_import_no_abi4_functions`.
- [x] R2 The chunk wire format is a 12-byte header (version, first/last
  flags, `u16` frame sequence, `u32` byte offset, `u32` frame length, all
  little-endian) followed by at least one data byte lying inside the frame;
  truncated, unknown-version, inconsistent-flag, empty and out-of-frame
  chunks are refused; a chunk of 166 data bytes fits one ESP-NOW message —
  `stream::chunk::tests::*` (`layout_and_round_trip`,
  `rejects_truncated_version_and_flags`,
  `rejects_zero_length_and_out_of_frame_data`, `write_needs_room`,
  `seq_is_little_endian_and_full_range`); `stream::tests::espnow_budget`;
  compile-time assertion against `espnow::MAX_PAYLOAD` with `espnow`.
- [x] R3 The source chunker splits a frame of `1..=MAX_FRAME_BYTES` bytes
  into chunks of `1..=MAX_CHUNK_DATA` bytes, any chunk computable directly
  from its index, and its output round trips through a sink into the
  original frame —
  `stream::source::tests::*` (`splits_into_full_chunks_and_a_short_tail`,
  `exact_multiple_and_single_chunk`, `rejects_bad_sizes`, `seq_wraps`,
  `chunker_output_feeds_a_sink_end_to_end`).
- [x] R4 The sink state machine (state in, `Step` out) forwards a chunk only
  when it is a new frame's first chunk (preamble first) or the exact
  continuation of the frame in progress, so loss, reordering, duplicates,
  sequence wrap, wrong frame sizes, zero-length and overrunning chunks never
  put a byte in the wrong place —
  `stream::sink::tests::*` (`in_order_frame_completes`, `single_chunk_frames`,
  `loss_of_a_middle_chunk_abandons_the_frame`,
  `loss_of_the_first_chunk_skips_the_frame`,
  `loss_of_the_last_chunk_is_abandoned_by_the_next_frame`,
  `duplicates_are_dropped_without_disturbing_the_frame`,
  `reordering_never_forwards_a_misplaced_chunk`,
  `exhaustive_small_permutations_never_misplace`, `seq_wrap_is_a_new_frame`,
  `wrong_size_frames_are_dropped`,
  `malformed_zero_length_and_oversize_chunks_are_dropped`,
  `receive_returns_the_data_to_send`,
  `source_restart_with_the_same_seq_loses_one_frame`);
  `stream::tests::stats_count_steps_and_saturate`, `source_stats`.
- [x] R5 The fake camera is a deterministic RGB565 big-endian test pattern
  (frame-number band, moving bar, shifting gradient) that differs every
  frame and can be generated for any byte range, so a host fills one chunk
  at a time without a frame buffer — `stream::pattern::tests::*`.
- [x] R6 Route arguments are validated in one place: source kind, source
  config (exactly 10 bytes: width and height `1..=320`, RGB565 or JPEG (R14), flags 0,
  period ≥ 10 ms, chunk size `1..=500` or 0 for 166, frame ≤ 153 600
  bytes), sink config (D/C pin, flags 0, frame length, up to 8 preamble
  segments of D/C level and bytes, at most 64 bytes, nothing trailing), the
  MIPI-DCS window helper, stat selectors and handles `0..4` —
  `wasm_abi::route::tests::*`.
- [x] R7 Routes report through `on_irq` with `RouteFrames` (5),
  `RouteDropped` (6) and `RouteStopped` (7) events; frame counts coalesce
  per route so a stream occupies one queue slot —
  `wasm_abi::events::tests::route_events_coalesce_per_handle_and_stops_do_not`.
- [x] R8 `wasm-host` implements the route imports through `Device` methods
  defaulting to `Unsupported`, checks source kind or bus and config length
  before pointers, never passes bad arguments to the device, maps a device
  handle outside `0..4` to `NoResources`, saturates counters at `i32::MAX`
  and dispatches route events to `on_irq` —
  `abi::tests::route_source_checks_arguments_before_pointers`,
  `route_sink_checks_arguments_before_pointers`; `wasm-host/tests/abi4.rs`
  (`r8_*`).
- [x] R9 The payload SDK exposes routes safely (`route::Route::source`,
  `Route::sink`, `stat`, `stop`, `on_event`; `irq::Event::RouteFrames`,
  `RouteDropped`, `RouteStopped`) with no new `unsafe` (the route imports
  only read guest memory), and `abi-check` treats `route_source` and
  `route_sink` as event sources that require `on_irq` —
  `mw-payload` `route::tests::*`, `irq::tests::decodes_route_events`;
  `abi-check` `tests::route_starters_require_on_irq_and_stop_stat_do_not`.
- [ ] R10 The desktop and browser simulators run source and sink routes
  (test pattern source; a sink that records or renders the frames), so the
  camera and display payloads run without hardware.
  - [x] Desktop (`payload-sim`): a test-pattern source route publishes each
    frame on the simulator clock (real and `--virtual`) as `Chunker`
    chunks filled by `stream::pattern`, the camera is `Unsupported`, and
    late clocks count skipped frames; a sink route on an open SPI bus owns
    the bus and D/C pin, steps a `stream::Sink`, sends the preamble and
    forwarded data to a GC9A01-like 240×240 panel model (`CASET`, `RASET`,
    `RAMWR`, RGB565 big-endian), raises `RouteFrames` / `RouteDropped`,
    and keeps `route_stat` counts; `--peer` runs two payloads over a
    shared in-memory bus, `--loss`/`--seed` drop messages reproducibly,
    and `--dump-panel` writes the panel as PPM. Lossy delivery never
    writes a pixel outside the window or one that belongs to no frame —
    `tools/payload-sim/tests/r10.rs`
    (`r10_camera_and_display_payloads_stream_the_pattern_onto_the_panel`,
    `r10_lossy_bus_never_draws_misplaced_pixels`,
    `r10_real_time_routes_follow_the_wall_clock`); `sim::tests::*`
    (`r10_source_chunks_drive_the_sink_onto_the_panel`,
    `r10_lossy_delivery_never_misplaces_pixels`,
    `r10_late_clock_skips_source_frames`); `sim::route::tests::*`,
    `sim::panel::tests::*`, `sim::bus::tests::*`.
  - [ ] Browser (`web/protocol-wasm`): not yet; its device still answers
    the route imports with `Unsupported`.
- [ ] R11 ESP32-CAM firmware: a test-pattern source route publishes
  120×120 frames on `app/cam/frame` over ESP-NOW from DMA-capable buffers;
  later the OV2640 behind the same `SourceKind::Camera`.
  - [x] ESP-NOW in the ESP base firmware (feature `espnow`, ESP32 and
    ESP32-S3; esp-radio 1.0.0-beta.1, ported from the conagotchi badge):
    the Wi-Fi station sits on fixed channel 1 and the middleware
    `EspNowTransport` filters `Prefix("app")`; payload publishes on
    `app/...` keys are broadcast, received `app/...` samples are delivered
    to subscriptions or sink routes, and both queues drop and count when
    full — `esp_base::radio::tests::*`
    (`r11_r12_only_app_keys_go_over_the_air`,
    `r11_r12_counters_saturate_and_report_on_new_losses`).
  - [x] Source route core: the first frame is due at start and each next
    one a period later; chunks go back to back, each broadcast awaited to
    its send callback (one frame queued in the driver at a time); a late
    frame starts at once and whole missed periods are skipped as dropped
    frames (only the latest is generated); a failed broadcast abandons the
    frame; each chunk's ESP-NOW frame (radio header, key, chunk header,
    pattern bytes generated in place, no frame buffer) decodes back to the
    pattern; routes take turns; results of stopped or restarted routes are
    ignored; chunks over 167 bytes, non-`app` keys and `Camera` are
    `Unsupported` — `esp_base::route::source::tests::*`
    (`r11_first_frame_is_due_at_start_then_every_period`,
    `r11_chunks_follow_back_to_back_within_a_frame`,
    `r11_a_late_frame_starts_at_once_and_whole_missed_periods_are_dropped`,
    `r11_a_failed_broadcast_abandons_the_frame`,
    `r11_clock_wrap_and_large_chunks`,
    `r11_write_frame_builds_a_radio_frame_with_the_pattern_in_place`,
    `r11_written_frames_rebuild_the_pattern_through_a_sink`,
    `r11_write_frame_refuses_what_does_not_fit`);
    `route::sources::tests::*` (`r11_idle_wait_and_send`,
    `r11_routes_take_turns`,
    `r11_stopped_or_restarted_routes_ignore_late_results`);
    `route::tests::r11_r12_reports_become_route_events`.
  - [ ] On the ESP32-CAM: `cam_stream` streams the pattern over the air
    (`./build.sh esp32` builds; not yet run on hardware); the OV2640
    behind `SourceKind::Camera`.
- [ ] R12 Conagotchi badge firmware (ESP32-S3, GC9A01 on SPI2: SCK 12,
  MOSI 11, CS 10, DC 13, RST 14): a sink route DMAs received chunks to the
  panel with the preamble re-sent at each frame start; the payload
  initialises the panel over `spi_open`/`spi_transfer`/GPIO.
  - [x] Sink route core: a frame start sends every preamble segment with
    D/C at its level and then the data with D/C high, a continuation sends
    data only, dropped chunks send nothing and gaps abandon the frame, at
    most `MAX_TRANSFERS` transfers per chunk; one sink per bus, D/C pin
    and key, handles `0..4` (`NoResources` when full, `Busy` for a handle
    not running) — `esp_base::route::sink::tests::*`
    (`r12_frame_start_sends_the_preamble_with_dc_levels_then_data`,
    `r12_dropped_chunks_make_no_transfer_and_gaps_abandon_the_frame`,
    `r12_no_preamble_means_data_only_and_transfers_are_bounded`);
    `route::table::tests::*` (`r11_r12_handles_are_allocated_and_freed`,
    `r12_a_sink_owns_its_bus_pin_and_key`,
    `r12_sink_state_is_stored_and_long_keys_are_refused`).
  - [ ] On the badge (`./build.sh esp32s3` builds; not yet run on
    hardware): chunks received over ESP-NOW or serial on the sink's key
    skip `on_message` and are DMAed from the received buffer to SPI2
    (`SpiBus::write_route`, no `DmaDone`); while the route runs
    `spi_open`/`spi_transfer` on bus 0 and GPIO calls on the D/C pin are
    `Busy` (a D/C pin the payload drives as an output is taken over);
    `route_sink` is `Busy` while a payload transfer is in flight; a failed
    transfer releases bus and pin and raises `RouteStopped`.
  - [x] Stopping a payload in any state (bus idle, payload transfer in
    flight, sink-route write mid-frame) cancels what runs and returns SPI2
    and its static DMA buffers to the hardware home, so the next payload
    (same boot) can `spi_open` again; starting while a transfer runs is
    `Busy` and keeps that transfer; a transfer that fails to start hands
    the parts back. The driver and buffers are always either idle or inside
    the running transfer (`esp_base::periph::dma_hw::DmaHw`) —
    `periph::dma_hw::tests::*` (`r12_stop_while_idle_returns_the_parts`,
    `r12_stop_during_a_payload_transfer_cancels_and_returns_the_parts`,
    `r12_stop_during_a_route_write_cancels_and_returns_the_parts`,
    `r12_starting_while_in_flight_is_busy_and_keeps_the_transfer`,
    `r12_a_transfer_that_fails_to_start_hands_the_parts_back`,
    `r12_parts_survive_any_number_of_payload_runs`).
- [ ] R13 Camera and display payloads (Rust SDK) and an end-to-end run
  between the two devices.
  - [x] `payloads/examples/cam_stream` (ESP32-CAM): a `TestPattern`
    source route on `app/cam/frame`, 120×120 RGB565 every 200 ms in
    166-byte chunks (174 ESP-NOW messages a frame, 870 a second, within a
    1000-a-second budget); route events counted and published on
    `app/cam/stats` (three `u32` LE); a stopped route restarted at once,
    else on every tick; `SourceKind::Camera` is a one-constant change —
    `cam_stream` `tests::*` (`config_bytes_match_the_abi_example`,
    `a_frame_is_174_espnow_messages_within_budget`,
    `source_is_the_test_pattern_until_the_camera_lands`,
    `events_accumulate_and_a_stop_asks_for_a_restart`, `counters_saturate`,
    `stats_encoding_is_three_le_words`, `host_stub_start_fails_cleanly`).
  - [x] `payloads/examples/cam_view` (conagotchi badge): opens SPI2 at
    40 MHz and the D/C and reset pins, runs the `mipidsi` 0.10 GC9A01 init
    sequence with the badge's inversion-on and BGR options (each command
    with D/C low, its parameters with D/C high, one transfer per
    `DmaDone`, delays on a one-shot timer), clears the panel black in 30
    padded transfers, then starts a sink route with D/C 13, 28 800-byte
    frames and `Preamble::mipi_window(60, 60, 179, 179)`; a failed step or
    a stopped route backs off 1 s and repeats the bring-up, giving up after
    5 failures in a row. The decisions are pure state machines
    (`Sequencer`, `Controller`) —
    `cam_view` `tests::*` (`init_sequence_matches_mipidsi_gc9a01_with_the_badge_options`,
    `reset_then_power_up_delay_come_first`,
    `sleep_out_waits_120_ms_before_display_on`,
    `clear_fills_the_whole_panel_in_bounded_transfers_after_ramwr`,
    `sequencer_ignores_unawaited_events_and_fails_on_dma_errors`,
    `sink_config_bytes_match_the_abi_example`,
    `controller_boots_reopens_and_starts_the_sink`,
    `route_stop_backs_off_then_redoes_the_bring_up`,
    `repeated_failures_give_up`, `a_started_route_resets_the_failure_count`,
    `stats_accumulate_and_saturate`,
    `host_stub_bring_up_backs_off_instead_of_panicking`).
  - [x] Both validate against the ABI and `cam_view` fits the badge
    (1-byte pages, 16 KiB memory, 4 KiB stack, 13 154-byte image,
    reproducible) — `abi-check` `tests/payloads.rs` (`rust_cam_stream_example`,
    `rust_cam_view_example`, `cam_view_fits_the_badge`,
    `p5_forbid_unsafe_code_examples_export_the_abi`);
    `scripts/check-all.sh`.
  - [ ] End-to-end run between the ESP32-CAM and the badge (needs R11 and
    R12). In `payload-sim` (`--peer`, `--virtual`, 50 000 fuel, 16 KiB
    memory) the pair brings the panel up in 330 ms and draws every frame
    in the centre window; the most fuel any call needed was 27 971
    (`cam_view`'s `init`) and 6 637 for `cam_stream`.
- [ ] R14 The camera stream is baseline JPEG: a source config may say
  `PixelFormat::Jpeg`; until the OV2640 driver lands, the fake camera
  serves baked JPEG frames, and the badge decodes them in WASM.
  - [x] ABI: format byte 1 is JPEG; width and height (`1..=320`) describe
    the image, the period and chunk rules are those of RGB565, and the
    frame length varies per frame (each chunk header's `total`,
    `1..=MAX_FRAME_BYTES`), so `SourceConfig::fixed_frame_len` is `None`;
    a 240×240 JPEG source at 200 ms encodes as `[240, 0, 240, 0, 1, 0,
    200, 0, 166, 0]`; the host accepts it —
    `wasm_abi::route::tests::jpeg_source_config`,
    `source_kinds_and_formats`, `source_config_rules`;
    `wasm-host` `abi::tests::route_source_checks_arguments_before_pointers`.
  - [x] Fixtures: 8 frames of a moving scene (gradient shifting per frame,
    a bar crossing the image once per loop, the frame number as a
    seven-segment digit and as bits in a counter band) at 240×240 and
    120×120 in `fixtures/cam/jpeg<size>/frame_<nn>.jpg`, baseline JPEG
    YCbCr 4:2:0, quality 60, no restart markers, no APPn, 4.1–4.7 KB (25–29
    chunks of 166 bytes) at 240×240 and 2.1–2.4 KB at 120×120. They are
    produced by an integer-only encoder (`tools/jpeg-fixtures`), and a
    fresh run reproduces the committed bytes exactly —
    `jpeg-fixtures` `tests/fixtures.rs`
    (`committed_fixtures_match_a_fresh_run`,
    `generation_is_deterministic_and_frames_differ`); `encoder::tests::*`
    (`zigzag_starts_and_ends_right_and_is_a_permutation`,
    `huffman_tables_are_complete_and_cover_every_symbol`,
    `fdct_matches_the_reference_dct`,
    `quantize_rounds_half_away_from_zero_and_tables_scale`,
    `categories_and_magnitudes`, `bit_writer_stuffs_and_pads`,
    `writes_a_baseline_420_stream`); `scene::tests::*`;
    `stream::frames::fixtures::tests::*`
    (`frames_are_baseline_420_jpegs_of_their_size`,
    `sizes_and_chunk_counts`, feature `cam-fixtures`).
  - [x] Fake JPEG camera core (`stream::frames`, `no_std`, no heap): a
    `FrameSet` of stored frames is checked once (non-empty, each
    `1..=MAX_FRAME_BYTES`) and loops (frame `i` is `frames[i % n]`); a
    `FrameSource` (pattern or set) gives each frame's length, its
    `Chunker` and any byte range written into a caller's buffer, so the
    chunks of every frame rebuild it with its own `total`; for
    `TestPattern`, RGB565 serves the pattern and JPEG the fixture set of
    that size (`InvalidArgument` for another size, `Unsupported` without
    `cam-fixtures`) — `stream::frames::tests::*`
    (`sets_are_checked_once`, `frames_loop_and_vary_in_length`,
    `fill_copies_any_range_and_stops_at_the_end`,
    `pattern_source_matches_the_pattern`,
    `chunks_rebuild_each_stored_frame`,
    `test_pattern_picks_the_source_by_format`).
  - [x] `payload-sim`: a JPEG `TestPattern` source publishes the fixture
    frames in order, looping, one per period, every chunk carrying its
    frame's length; other JPEG sizes are `InvalidArgument` —
    `sim::route::tests::jpeg_chunks_are_the_fixture_frames_in_order`;
    `sim::tests::r10_jpeg_source_publishes_the_fixture_frames_in_order`.
  - [x] ESP base firmware (ESP32, ESP32-S3; feature `cam-fixtures`): each
    frame gets its own chunker, frames vary in length and loop, skipped
    periods jump through the loop, and each chunk is copied from the
    flash-resident frame straight into the ESP-NOW frame buffer (no frame
    buffer); a chunk past the stored frame is refused; JPEG without the
    feature is `Unsupported` — `esp_base::route::source::tests::*`
    (`r11_jpeg_frames_vary_in_length_and_loop`,
    `r11_jpeg_skipped_periods_jump_through_the_loop`,
    `r11_jpeg_write_frame_copies_the_stored_bytes`,
    `r11_test_pattern_jpeg_needs_the_fixtures`, run with and without
    `--features cam-fixtures`); `./build.sh esp32` and `./build.sh
    esp32s3` build (about 54 KB more flash).
  - [x] Payloads: `cam_jpeg_stream` (ESP32-CAM; `cam_stream` stays
    RGB565) starts a `TestPattern` source with `SourceConfig::jpeg(240,
    240, 100)` on `app/cam/jpeg`: the fixtures' 25–29 chunks a frame make
    at most 290 ESP-NOW messages a second, within the 1 000-a-second budget
    (a frame may grow to 100 chunks); stats on `app/cam/stats`, restart on
    `RouteStopped` — `cam_jpeg_stream` `tests::*`
    (`config_bytes_are_a_240_jpeg_at_100_ms`,
    `fixture_frames_are_25_to_29_chunks_within_budget`,
    `source_is_the_test_pattern_until_the_camera_lands`,
    `events_accumulate_and_a_stop_asks_for_a_restart`,
    `stats_encoding_is_three_le_words`, `host_stub_start_fails_cleanly`).
    `cam_jpeg_view` (badge) brings the panel up with the shared `gc9a01`
    crate (moved out of `cam_view`, whose tests still pass), reassembles
    `app/cam/jpeg` into three 8 KiB slots (fill, newest complete,
    decoding: in-order continuation only, a gap or a new first chunk drops
    the frame in progress, a complete frame not yet decoded is superseded
    by a newer one, the decoding slot is never written), decodes the newest
    frame with `mw-jpeg` four MCUs per call, and sends each strip as
    `CASET`/`RASET` (D/C low command, D/C high parameters), `RAMWR` and the
    pixels in transfers of at most 3 840 bytes, one in flight at a time;
    stats on `app/cam/view_stats` — `cam_jpeg_view` `reasm::tests::*`
    (`in_order_frames_complete_and_are_taken_newest_first`,
    `a_gap_abandons_the_frame_and_its_rest_is_dropped`,
    `a_new_first_chunk_abandons_the_frame_in_progress`,
    `duplicates_are_dropped_without_losing_the_frame`,
    `oversize_malformed_and_foreign_chunks`,
    `single_chunk_frames_and_counters_saturate`,
    `lossy_reordered_delivery_only_ever_yields_whole_frames`),
    `pipeline::tests::*` (`a_full_width_strip_is_window_ramwr_and_two_parts`,
    `smaller_images_are_centred_and_parts_never_exceed_max_dma`,
    `errors_abandon_the_frame`, `events_out_of_place_are_ignored`),
    `tests::*` (`memory_fits_one_page_with_the_stack`,
    `stats_fold_the_reassembler_and_encode_as_le_words`,
    `received_fixture_frames_decode_in_bounded_steps`,
    `host_stub_bring_up_backs_off_and_frames_still_reassemble`);
    `gc9a01` `tests::*`; `abi-check` `tests/payloads.rs`
    (`rust_cam_jpeg_stream_example`, `rust_cam_jpeg_view_example`,
    `cam_jpeg_view_fits_one_standard_page`,
    `p5_forbid_unsafe_code_examples_export_the_abi`);
    `scripts/check-all.sh`.
  - [x] `payload-sim` draws a payload's own SPI: `--panel-dc <pin>` feeds
    `spi_transfer`s on bus 0 (padded to the clocked length) to the panel
    with the D/C level of that GPIO output, and every board prints its
    fuel use (calls, most in one call, total; `wasm_host::FuelStats`) —
    `sim::tests::*` (`panel_dc_payload_spi_draws_on_the_panel`,
    `panel_dc_padding_bytes_are_pixel_data`,
    `panel_dc_pin_not_an_output_does_not_feed_the_panel`,
    `without_panel_dc_payload_spi_leaves_the_panel_alone`,
    `long_spi_transfers_render_their_head_only`); `tests/panel_dc.rs`;
    `tests/d10.rs` `d10_run_ends_with_the_payload_fuel_use`; `wasm-host`
    `tests/runtime.rs` (`w4_fuel_stats_record_every_call`,
    `w4_fuel_stats_count_a_dry_call_as_its_whole_budget`).
  - [x] End-to-end in `payload-sim` (`--peer`, `--virtual`, 1 000 000 fuel,
    64 KiB): the panel equals `mw-jpeg`'s host decode of the fixture frame
    last drawn, bit for bit; every frame after the bring-up is drawn; no
    call traps and the most any call needs is 294 354 fuel (about 15 M per
    frame in all); with 2 % message loss frames are dropped, none fails,
    and every 16-row band is black or a band of a fixture frame —
    `tools/payload-sim/tests/r14.rs`
    (`r14_jpeg_stream_is_decoded_bit_exactly_onto_the_panel_within_the_fuel_budget`,
    `r14_lossy_stream_drops_frames_without_trapping_or_misdrawing`).
  - [x] On hardware (ESP32-CAM to badge): the badge decodes and draws the
    stream (`cam_jpeg_view: frame N drawn`); since R15 the frames are the
    OV2640's, behind `SourceKind::Camera` (manual hardware run, see R15).
- [ ] R15 The real camera: the ESP32-CAM's OV2640 behind
  `SourceKind::Camera`. The payload sets the sensor up (power, XCLK on
  PWM, SCCB registers over I2C); the host only receives what the sensor
  clocks out, by DMA, and publishes each JPEG through the source-route
  chunker.
  - [x] ABI: a camera `route_source` config is 21 bytes, the JPEG
    `SourceConfig` then `CameraPins` (D0..D7, VSYNC, HREF, PCLK, distinct);
    the test pattern keeps exactly 10; anything else, a camera config
    that is not JPEG, or repeated pins is `InvalidArgument` before the
    device is called; `Device::route_source` receives a `Source` carrying
    the pins; the PWM ceiling is 40 MHz —
    `wasm_abi::route::tests::camera_config_layout_and_rules`;
    `wasm_abi::periph::tests::pwm_rules`; `wasm-host`
    `abi::tests::route_source_checks_arguments_before_pointers`,
    `tests/abi4.rs` `r15_camera_source_passes_its_pins_and_needs_21_bytes`,
    `tests/abi3.rs` `x3_pwm_open_set_stop_and_bad_args`; `mw-payload`
    `route::tests::camera_needs_jpeg_and_distinct_pins_and_reaches_the_host`,
    `pwm::tests::open_validates_before_the_host`.
  - [x] Board pin map and claims (ESP32 = ESP32-CAM): PWM timer 0
    channels 0..3 on GPIO 0, 4, 13, 14, I2C bus 0 on SDA 26 / SCL 27, a
    camera port; other chips none (`Unsupported`; since E24 the ESP32-S3
    maps I2C bus 0 to SDA 5 / SCL 6); PWM, I2C and camera
    pins are owned exclusively, checked in their direction (camera inputs
    may be input-only), refused to `gpio_*`, and released per owner; the
    LEDC plan gives each frequency the most duty bits its divider allows
    (2 at 20 MHz) and rounds the 16-bit duty with exact ends —
    `esp_base::chip::tests::r15_board_pin_map_wires_the_esp32_cam_sensor`;
    `periph::claims::tests::r15_pwm_i2c_and_camera_pins_are_owned_and_exclusive`;
    `periph::pwm::tests::*`
    (`r15_camera_clocks_get_the_bits_the_divider_allows`,
    `duty_rounds_to_the_nearest_step_with_exact_ends`).
  - [x] Capture core (pure): the I2S camera-mode sample bytes are
    extracted from whole DMA words in both FIFO layouts; the assembler
    finds `FF D8` … `FF D9` across every split of the stream (stuffed
    `FF 00`, fill `FF FF` and restart markers inside), stops at the end of
    the frame, and reports a frame larger than the buffer as overflow and
    seeks again, never a truncated frame —
    `esp_base::camera::tests::*` (`r15_extract_takes_the_sample_bytes_of_whole_words`,
    `r15_assembler_finds_the_frame_in_one_feed`,
    `r15_assembler_is_split_proof`,
    `r15_assembler_reports_overflow_and_seeks_again`,
    `r15_assembler_through_the_dma_layout`).
  - [x] Camera route pacing: a due frame asks for a capture; its length
    becomes every chunk's `total`, the chunks are sent from the frame
    buffer, one sequence number per frame; a failed capture (overflow,
    timeout, hardware) drops the frame and whole missed periods; stale
    capture results are ignored; a camera route needs JPEG and chunks that
    fit one ESP-NOW message —
    `esp_base::route::source::tests::*`
    (`r15_camera_frames_are_captured_when_due_then_sent_from_the_buffer`,
    `r15_a_failed_capture_drops_the_frame_and_waits_a_period`,
    `r15_camera_routes_need_jpeg_and_air_sized_chunks`);
    `route::sources::tests::r15_camera_routes_ask_for_a_capture_then_send_its_chunks`.
  - [x] ESP32 shell: PWM on LEDC high-speed timer 0 (`LEDC_HS_SIG<n>` routed
    through the pin bank), I2C on I2C0 (open drain, pull-up), and I2S0 in
    camera mode with `DMA_I2S0` (receive FIFO mode 1, two samples per
    32-bit word; data lines on `I2S0I_DATA_0..7` and `8..15`, HREF on
    `H_ENABLE`, PCLK on `WS`, `H_SYNC`/`V_SYNC` held high, VSYNC claimed)
    configured through esp-hal's I2S driver plus single-bit PAC fields, so
    no `unsafe`; a 16 KiB DMA ring and an 8 KiB frame buffer allocated
    once from the heap at the first camera route (`NoResources` if short);
    capture in the radio task, bounded by 600 ms, yielding while the ring
    is empty; overflow and overrun drop the frame; the first captured
    frame of a boot and every 64th are mirrored on the serial links as
    `dbg/cam/jpeg` chunks — `./build.sh esp32` and `esp32s3` build, both
    chips' clippy is clean; verified on hardware (below).
  - [x] Payload `cam_ov2640_stream` (ESP32-CAM): PWDN (GPIO 32) low, XCLK
    20 MHz on PWM timer 0 channel 0, I2C bus 0 at 100 kHz; a pure
    sequencer then waits 10 ms, reads PID/VER (two transfers, no repeated
    start) and logs them, soft-resets (300 ms), writes the 219-entry table
    in slices of at most 32 writes per call — Zephyr's `default_regs`
    (Apache-2.0), JPEG at `QS` 15, UXGA input cropped to the centred
    1200×1200 square (offset x 200) scaled to 240×240, `CLKRC` 0x00 and
    `R_DVP_SP` 0x08 (esp32-camera's JPEG clock; with the doubler and auto
    divider the PCLK outran the ESP32's I2S and bytes were lost) — waits
    30 ms and starts `Route::camera` on `app/cam/jpeg` every 500 ms
    (2 frames/s; an 8 KiB frame is 50 chunks, 100 ESP-NOW messages a
    second); a wrong ID or bus error backs off 100–800 ms and restarts,
    giving up after 5 in a row; a stopped or refused route retries on the
    2 s stats tick; stats on `app/cam/stats`. Deploy build: byte pages,
    6 144 bytes of memory, 7 801-byte image — `cam_ov2640_stream`
    `bringup::tests::*`, `ov2640::tests::*`, `tests::*` (27, e.g.
    `happy_path_order_id_then_reset_then_whole_table_then_route`,
    `wrong_id_backs_off_without_resetting_and_gives_up`,
    `a_bus_error_mid_table_restarts_from_identify_and_rewrites_everything`,
    `window_registers_for_240x240`, `every_pclk_setting_is_the_esp32_one`,
    `camera_config_is_the_21_bytes_with_the_ai_thinker_pins`,
    `an_8k_frame_at_500_ms_stays_within_the_espnow_budget`,
    `host_stub_init_fails_cleanly`); `abi-check`
    `rust_cam_ov2640_stream_example`,
    `cam_ov2640_stream_deploy_build_fits_the_esp32`; `check-all.sh`
    (reproducible build).
  - [x] `payload-sim`: an OV2640 SCCB model at 0x30 on I2C bus 0 (banks,
    PID 0x26 / VER 0x42, COM7 soft reset); the camera route stays
    `Unsupported` (no image sensor model); the payload's whole bring-up
    runs against it (222 writes, final registers 240×240 JPEG, 5 917 fuel
    at most in one call) and retries the refused route without trapping —
    `sim::periph::ov2640::tests::*`, `sim::periph::tests::i2c_bus_0_carries_the_ov2640`,
    `tests/ov2640.rs` `ov2640_bring_up_writes_the_whole_table_and_tries_the_camera_route`;
    `tests/r10.rs` (camera still `Unsupported` with a valid 21-byte config).
  - [x] Badge viewer: frames change size from frame to frame and smaller
    ones are centred; a smaller frame after a larger one first blacks out
    the bands it does not cover (in transfers of at most 3 840 bytes), so no
    stale pixels remain; frames wider or taller than the panel are refused
    and the next drawn — `cam_jpeg_view` `pipeline::tests::*`
    (`bands_and_image_tile_the_panel_exactly`,
    `a_smaller_frame_after_a_larger_one_blacks_out_its_border_first`,
    `size_changes_between_frames_draw_each_frame_in_its_own_window`,
    `a_frame_larger_than_the_panel_is_refused_and_the_next_one_drawn`),
    `tests::frames_changing_size_are_each_drawn_centred_on_black`. Image
    25 714 → 26 353 bytes, memory unchanged.
  - [ ] On hardware (manual run, not regression-tested): ESP32-CAM logs
    PID 0x26 VER 0x42, captures 240×240 JPEGs of 2–5 KB in 80–140 ms
    (DMA 4–12 KB per capture), streams them at 2 frames/s; the badge
    decodes and draws them in about 2.0 s each; a mirrored frame decodes
    with PIL to a 240×240 image. Open: re-verify after the final flash.

### Serving requests (ABI 5, M…)

Payloads answer requests, not only publish and subscribe, so a unit's
payload can implement a request contract such as the S88 phase commands
(`unit/<node>/phase/<em>.<phase>`). A payload registers a key with
`serve`, learns its device's node id with `node_id`, and answers in its
`on_request` export; the device routes requests on served keys to
`wasm_host::Payload::request` and answers the caller with the reply or
the payload's `HandlerError`. Nothing is allocated per request: the
payload's `mw_alloc` provides one buffer holding a 16-byte meta block
(sender and deadline; never a principal), the body and the reply space.
The contract is `wasm_abi::serve`, pure and heap-free.

- [x] M1 ABI 5 is a superset of ABI 4: `serve` (2 params) and `node_id`
  (1) are appended to `wasm_abi::IMPORTS`, `on_request` is an optional
  export of six `i32` parameters and an `i32` result, `ABI_VERSION` is 5,
  ABI 1–4 modules load unchanged, and an ABI 4 host (the ABI 4 prefix)
  does not know the new imports, so such a payload fails to load with an
  unknown import instead of running half wired —
  `wasm_abi::tests::revisions_are_prefixes_of_the_import_table`,
  `an_abi4_host_does_not_know_the_abi5_imports`;
  `wasm-host/tests/abi4.rs` `r1_every_import_links_and_older_revisions_still_load`;
  `wasm-host/tests/abi5.rs` `s10_abi5_imports_are_unknown_to_an_abi4_host`.
- [x] M2 The request rules are pure and tested without an interpreter:
  the buffer layout (meta, body, reply), the meta block encoding (flags,
  deadline, sender; unknown flags and short blocks refused), the result
  code (a length within the offered capacity, a negated `HandlerError`
  code, anything else `Internal`), the served-key table and the
  `node_id` target range — `wasm_abi::serve::tests::*`;
  `wasm-host` `abi::tests::abi5_serve_slots_node_id_and_request_ranges`,
  `link::tests::serve_needs_request_exports_and_the_device`,
  `node_id_checks_the_range_and_the_device`.
- [x] M3 `wasm-host` serves a request end to end: the body and meta reach
  the slot, the reply comes back; an unserved key is `NotFound` and an
  oversized body `InvalidRequest` without calling the payload; a declined
  or out-of-memory buffer is `Unavailable`; a reply longer than offered or
  an unknown code is `Internal` and copies nothing; running out of fuel in
  `on_request` traps and poisons; `serve` checks keys, bounds, the eight
  slots, the exports and the device; a board without the ABI 5 methods
  answers `Unsupported` — `wasm-host/tests/abi5.rs` `s1`–`s9`.
- [x] M4 `payload-sim` takes `!req <key> [<hex>]`, hands it to the board
  serving the key and prints the reply or the error; `--node` sets the
  node id — `payload-sim` `sim::tests::abi5_req_commands_node_option_and_reply_lines`;
  `tools/payload-sim/tests/abi5.rs` `s11_req_reaches_the_serving_payload_and_prints_the_reply`,
  `s11_the_default_node_is_one`.
- [x] M5 The Rust SDK serves safely: `serve::serve`, `serve::node_id`
  (the one new `unsafe` call site, writing exactly an owned 8-byte
  array), `Payload::on_request` with a `Request` (body, meta) and a reply
  slice, and `payload!(App, serve)` whose `on_request` only accepts
  regions inside its receive buffer; `payload!(App)` modules are unchanged
  — `mw-payload` `rt::tests::on_request_answers_from_the_receive_buffer`,
  `request_parts_bounds_every_region`, `alloc_follows_the_buffer_size`,
  `serve::tests::slots_hex_and_host_stubs`; example `serve_echo`
  (`tests::answers_node_then_body_or_buffer_too_small`); `abi-check`
  `tests::abi5_serve_needs_on_request_and_mw_alloc`, `tests/payloads.rs`
  `rust_serve_echo_example`.
- [x] M6 AssemblyScript authors have the same contract: `templates/serve.ts`
  and the template README's ABI 5 section, compiled and validated by
  `payloads/scripts/check-all.sh` — `abi-check` `tests/payloads.rs`
  `as_serve_template`.
- [ ] M7 Firmware routes requests on served keys to the payload (esp-base,
  rp-base, conagotchi): `Device::serve` registers the key with the
  device's responder, `node_id` returns the `sys/info` node id, and the
  shell answers with `Payload::request`'s reply (a trap as `Internal`).

### JPEG decode in WASM (J…)

`payloads/jpeg` (`mw-jpeg`): the badge-side decoder of R14, a `no_std`,
heap-free, float-free, `#![forbid(unsafe_code)]` library a payload links.
A frame reassembled in linear memory is decoded in bounded steps under
wasmi's per-call fuel budget into RGB565 strips for the GC9A01.

- [x] J1 Baseline sequential Huffman JPEG, 8-bit samples (SOF0, and SOF1
  within the same limits): grayscale and YCbCr 4:4:4, 4:2:2 (h2v1) and
  4:2:0 (h2v2), any size from 1×1 to `MAX_DIMENSION` (2048) including
  sizes that are not a multiple of the MCU; APPn/COM skipped, a missing
  EOI accepted. Decoded output matches a floating point reference decode
  within a mean of 0.3 and a maximum of 3 levels per channel (observed
  0.17 and 3), at qualities 30, 75 and 95 and sizes up to 320×240 —
  `tests/codec.rs` (`full_scale_matches_reference_for_every_layout_size_and_quality`,
  `header_reports_geometry`, `missing_eoi_is_accepted`).
- [x] J2 Interoperability with libjpeg: Pillow-encoded fixtures decode
  bit-exactly for 4:4:4 and grayscale, and within 3 levels of ffmpeg's
  planes with replicated chroma for 4:2:2 and 4:2:0 (chroma is replicated,
  not interpolated like libjpeg's "fancy" upsampling); the fake camera's
  baked frames decode — `tests/fixtures.rs` (`libjpeg_444_matches`,
  `libjpeg_gray_matches`, `libjpeg_422_matches`, `libjpeg_420_matches`,
  `libjpeg_420_with_restart_markers_matches`, `baked_camera_frames_decode`).
- [x] J3 Restart intervals (DRI, RST0..RST7 in sequence; a wrong marker is
  `Malformed::Restart`) — `tests/codec.rs`
  (`restart_intervals_decode_like_the_reference`,
  `wrong_restart_marker_is_malformed`); `bits::tests::restart_checks_number`.
- [x] J4 Everything else is a clear error from `Decoder::new`:
  `Unsupported::{Progressive, Lossless, Hierarchical, Arithmetic,
  Precision, Components, Sampling, MultiScan, QuantPrecision, TableSlot,
  Dnl, TooLarge}` and `Malformed::*` for broken syntax — `tests/codec.rs`
  (`unsupported_and_malformed_headers`, `four_components_are_unsupported`,
  `arithmetic_conditioning_segment_is_unsupported`); `tests/fixtures.rs`
  (`libjpeg_progressive_is_unsupported`);
  `huffman::tests::rejects_overfull_and_mismatched_tables`.
- [x] J5 Incremental, fixed-size and borrowing: the `Decoder` struct
  (4 352 bytes on wasm32, independent of the image size; all-zero
  `Decoder::IDLE` for a `.bss` static, `start` re-fills it in place) holds
  tables, bit position, DC predictors, an MCU cursor and one MCU of
  samples; the JPEG slice is passed to each call (`InputChanged` if its
  length differs) and pixels go to the caller's strip buffer.
  `decode_blocks(n)`, `decode_mcus(n)` and `decode_mcu_row` return
  `Step::{Partial, Row(Strip), Done}` and give the same image for every
  step size; misuse (`BufferTooSmall`, `InputChanged`) leaves the state
  untouched — `tests/codec.rs` (`every_step_size_gives_the_same_image`,
  `idle_decoder_is_done_and_restarts_in_place`,
  `misuse_is_reported_without_losing_state`); `tests::decoder_size`
  (and a compile-time check in `examples/fuel_probe.rs`).
- [x] J6 Integer arithmetic: libjpeg's `jidctint` IDCT (within one level
  of the exact IDCT) and `jpeg_idct_4x4` for half scale, fixed-point
  YCbCr → RGB, RGB565 big-endian output (also RGB888) —
  `idct::tests::*` (`matches_float_idct_within_one_level`,
  `reduced_idct_is_the_2x2_average_of_the_full_idct`,
  `dc_only_block_is_flat`, `clamps_extremes`); `color::tests::*`;
  `tests/codec.rs` (`rgb565_is_rgb888_truncated_and_big_endian`);
  `tests/fixtures.rs` (`camera_frame_rgb565_matches_rgb888`).
- [x] J7 Half-scale output (`Scale::Half`, e.g. 240×240 → 120×120 for the
  centre window) within a mean of 0.5 and a maximum of 4 levels of the
  reference (observed 0.22 and 2) — `tests/codec.rs`
  (`half_scale_matches_reference`).
- [x] J8 Corrupt or truncated input never panics or loops: every loop has a
  fixed bound, arithmetic on coefficients wraps, and 10 000 seeded random
  mutations (byte flips, header flips, truncations, insertions, deletions,
  runs of `FF`/`00`) of valid files, decoded with overflow checks on at
  random scales, formats and budgets, end in `Done` or an error within one
  call per block; every truncation of a frame is an error, and a truncated
  frame keeps failing — `tests/fuzz.rs`
  (`random_corruptions_never_panic_or_hang`,
  `every_truncation_of_a_frame_is_handled`); `tests/codec.rs`
  (`truncated_data_is_an_error_that_sticks`);
  `idct::tests::huge_coefficients_do_not_panic`;
  `bits::tests::end_of_data_is_an_overrun_only_when_consumed`.
- [ ] J9 Fits the badge's budgets. Measured, not regression-tested
  (`examples/fuel_probe.rs`, wasmi with eager translation, 240×240 4:2:0):
  `start` 20 400 fuel; `decode_blocks(.., 1)` at most 41 200 (18 800 at
  half scale); a 4:2:0 MCU 74 500 on average (50 600 at half scale) on a
  noisy quality-80 frame and 56 600 (38 700) on the fake camera's
  quality-60 frames, i.e. about 0.8 M fuel per 240-wide MCU row. wasmi's
  lazy translation adds about 7 fuel per code byte to the first call that
  reaches a function (about 36 000 on the first `start`, 33 000 on the
  first decode call), more than a 50 000-fuel budget allows; `wasm-host`
  now translates eagerly at load (W10), so on it first calls cost no
  extra. Code size is
  about 14 KiB of wasm (target 12 KiB), the decoder state 4.3 KB, the
  4:2:0 strip 7 680 bytes at 240 wide.
- [x] J11 The badge payload (`cam_jpeg_view`) stays within its budgets:
  `decode_mcus(.., 4)` per call (worst case about 4 × 114 000 fuel on a
  noisy frame), `Decoder::start` in a call of its own, and every call of a
  run on the fixture stream under 600 000 fuel (294 354 measured) at the
  firmware's 1 000 000; one standard 64 KiB page holds the 16 KiB stack,
  three 8 KiB frame slots, the 7 680-byte strip and the decoder in `.bss`
  — `tools/payload-sim/tests/r14.rs`
  (`r14_jpeg_stream_is_decoded_bit_exactly_onto_the_panel_within_the_fuel_budget`);
  `cam_jpeg_view` `tests::received_fixture_frames_decode_in_bounded_steps`,
  `tests::memory_fits_one_page_with_the_stack`; `abi-check`
  `cam_jpeg_view_fits_one_standard_page`. (Now 259 985 fuel at most, see
  J12.)
- [x] J12 The badge deploys `cam_jpeg_view`'s byte-page build sized to
  its need, small enough that the firmware's heap check passes with room:
  memory fixed at 48 128 bytes (data and `.bss` 39 088 + 8 KiB stack,
  rounded up to 1 KiB; the fixture stream needs under 3 072 bytes of
  stack) from `[package.metadata.byte-pages]`, which
  `build-byte-pages.sh` reads; `mw-jpeg` without `half` (no reduced IDCT,
  `Unsupported::Scale` for `Scale::Half`), the SDK's panic-free paths
  (P7) and `opt-level = "s"` for the crate: image 30 878 → 25 714 bytes,
  23.3 KB of code, 13.2 M fuel per frame (15.0 M before; "z" for
  `mw-jpeg` measured 15.9 M and a larger image), 259 985 at most per
  call. Heap needed on the badge 164 291 bytes (W10) —
  `cam_jpeg_view` `tests::deploy_build_memory_is_whole_kib_and_holds_buffers_and_stack`;
  `abi-check` `cam_jpeg_view_deploy_build_declares_exactly_its_memory`;
  `tools/payload-sim/tests/heap.rs` (`cam_jpeg_view/b`); `mw-jpeg` tests
  with and without default features (`header_reports_geometry` checks the
  refusal); `payloads/scripts/check-all.sh` (builds it reproducibly and
  validates it).
- [x] J10 Built and checked with the payload workspace: unit, round-trip,
  interop and fuzz tests, clippy (wasm32 and host) and rustdoc with
  warnings denied, and the `fuel_probe` payload validated by `abi-check` —
  `payloads/scripts/check-all.sh`.

### S88 recipe model (Q…)

The payload editor began with one model per payload: a board, its pin
table and its logic. That suits one board doing one job. It does not
describe a procedure that spans several boards ("blink the status light,
then take a reading on the sensor board"), and it ties the procedure to
pins. The plant is now described the way ISA-88 (S88, IEC 61512-1) batch
control describes a plant: three models plus the procedural control
model. The rules live in the `recipe-core` crate (no_std, no heap, no
recursion, built for `thumbv6m-none-eabi` and for the browser through
protocol-wasm); the shapes live in the web model (`web/src/model/plant.ts`,
`procedural.ts`, `recipe.ts`).

From the top: the master recipe (process model, `recipe.ts`) names phase
classes; allocation rules (`recipe-core` `alloc`) instantiate it as a
control recipe (runtime binding, executed by the `recipe-core` engine over
the wire contract), which maps abstract phases to unit-specific phases;
those belong to the physical model of units, equipment modules and
control modules (`plant.ts`).

#### The models

- **Physical model.** A *unit* is one board: an id, a name, a *unit
  class* (an identifier recipes may constrain to), the node id
  `sys/info` reports once the unit is bound to a board (16 hex digits, or
  unbound), its board target, and its payload (Blocks workspace, flows,
  state machines). Its *control modules* are the rows of its pin table,
  checked by the firmwares' own pin rules as before. An *equipment
  module* is a named group of control modules on the unit (a control
  module belongs to at most one).
- **Procedural control model.** A *phase class* is the
  equipment-independent interface recipes ask for: an identifier and up
  to four whole-number parameters, each with a range (for example
  Blink(period_ms 10..10000, times 1..100), Measure(samples 1..16)). An
  *equipment phase* is a unit's implementation of a class on one of its
  equipment modules: a name, the range it accepts for every class
  parameter (inside the class's), and for each acting state of the phase
  state model optionally a flow of that unit's logic as the state's body.
  Every equipment phase runs the phase state model below on the unit.
- **Master recipe (process model).** A procedure of *unit procedures*
  (each optionally constrained to a unit class) and *steps*. A step runs
  one phase class with a value for every parameter inside one unit
  procedure; an *operation* label groups steps for the reader. Steps are
  joined by *transitions* into a sequential function chart. A step never
  names a board, a pin or a unit.
- **Control recipe.** A master recipe bound by allocation: each unit
  procedure to one unit, each step to one equipment phase of that unit.
  The engine executes it by commanding the bound phases.

Deviation from the coordinator's mapping: equipment phases live in the
procedural section of the document, keyed by unit and equipment module,
rather than inside each equipment module. Phase classes are shared by
recipes and units, so they cannot live in either; keeping the phases next
to them makes the procedural model one list the phase editor (round 8)
reads, and the references (unit, module, class, body flow) are checked
like any other.

#### Phase state model

The states and commands follow the example procedural-element state model
of ISA-88.00.01 §5.2 (IEC 61512-1, the figure of states and commands for
procedural elements). Pausing and Paused are left out: a phase here has no
breakpoints, and Hold covers stop-and-resume. Resetting is taken from the
PackML state model (ISA-TR88.00.02): ISA-88 resets straight to Idle, which
leaves the phase no state in which to put its equipment back. Every state
is *acting* (its logic runs and ends by reporting Done: Running, Holding,
Restarting, Stopping, Aborting, Resetting) or *quiescent* (Idle, Held,
Complete, Stopped, Aborted). Complete, Stopped and Aborted are *final*.
The recipe procedure follows the same model.

| state      | Start   | Hold    | Restart    | Stop     | Abort    | Reset     | Done     |
|------------|---------|---------|------------|----------|----------|-----------|----------|
| Idle       | Running |         |            |          |          |           |          |
| Running    |         | Holding |            | Stopping | Aborting |           | Complete |
| Complete   |         |         |            |          |          | Resetting |          |
| Holding    |         |         |            | Stopping | Aborting |           | Held     |
| Held       |         |         | Restarting | Stopping | Aborting |           |          |
| Restarting |         | Holding |            | Stopping | Aborting |           | Running  |
| Stopping   |         |         |            |          | Aborting |           | Stopped  |
| Stopped    |         |         |            |          |          | Resetting |          |
| Aborting   |         |         |            |          |          |           | Aborted  |
| Aborted    |         |         |            |          |          | Resetting |          |
| Resetting  |         |         |            | Stopping | Aborting |           | Idle     |

An empty cell rejects the input and leaves the state unchanged. States
have wire codes 0..10 in the order listed; commands 1..6 in column order.

#### Master recipe rules

The chart has no selection (alternative) divergence: a transition's only
condition is that its `from` steps' phases are Complete, a step ends at
most one transition and is started by at most one, and the chart is
acyclic. Parallel divergence (one transition starting several steps) and
convergence (one waiting for several) are allowed. The *initial* steps are
those no transition starts. These rules make the chart a marked graph in
which, by induction in topological order, every step runs exactly once
when every phase completes, which is the engine's termination argument.
Identifiers are unique ignoring case; arguments must name class
parameters, each once, every parameter given and in the class's range;
limits are 16 phase classes, 4 parameters, 4 unit procedures, 16 steps and
16 transitions. A unit procedure no step uses is a warning.

#### Allocation rules

A unit can host a unit procedure when it is online in discovery, is of
the unit class the procedure asks for (if any), and has, for every step of
the procedure, an equipment phase of the step's class whose ranges hold the
step's arguments. Among several such phases the smallest (equipment
module, phase name) by byte order wins; two steps that may be active at
the same time never share a phase, so the later one takes the next
suitable phase. Each unit hosts at most one unit procedure of a recipe (an
S88 unit runs one unit procedure at a time). Units are tried in byte order
of their ids and the first assignment in that order wins, so the result
depends only on ids and capabilities, never on listing order; the search is
an iterative depth-first search with a fixed move bound. A failure names
the first unit procedure no unit can host and, per unit, the first reason
(offline, wrong unit class, no phase of the class, an argument outside the
phase's range, every suitable phase taken by a concurrent step), or reports
contention when every procedure has candidates but there are too few
distinct units. Allocation assumes the bound phases are Idle.

#### Control-recipe engine

The engine is a pure state machine. Each input (an operator command, a
command's acknowledgement, a phase's state report, a unit leaving or
rejoining the discovery directory, or the passage of time) is applied at
the current time and yields the next engine and a bounded list of effects
(send a command to a phase, the procedure or a step changed, a command was
refused, a failure, an input ignored).

- **Steps.** Start activates the initial steps. An active step's phase is
  sent Start, with the step's arguments in class parameter order, once
  the phase is Idle; the step is *started* when that command is
  acknowledged and *done* when its phase reports Complete. A done step's
  phase is sent Reset to release it. A transition fires once, when all
  its `from` steps are done. The procedure is Complete when every step is
  done and every bound phase is back to Idle or unreachable.
- **Commands are level-triggered.** After every input the engine compares
  each bound phase's last known state with what the procedure's state asks
  for and sends at most one command per phase, only when none is
  outstanding, the unit is online, the state is known and the phase state
  model allows it. Holding or Held asks running phases to Hold; Restarting
  asks held phases to Restart; Stopping asks phases doing something to
  Stop; Aborting asks them to Abort; Resetting asks final phases to Reset.
  Hold, Restart, Stop and Abort go only to the phases of *involved* steps
  (active and started, or with a Start outstanding). The procedure moves
  on (Holding to Held, Stopping to Stopped, Aborting to Aborted, Restarting
  to Running, Resetting to Idle) when its involved phases have caught up.
- **Acknowledgement.** A command is acknowledged by its reply (accepted
  with the new state, or rejected with the current state) or by a state
  report naming its id, so a unit that can only publish still completes
  the handshake. Command ids increase; a report naming an id older than
  the last applied command is ignored as stale.
- **Failure policy.** A failure is *resumable* when the phase can still be
  held and restarted (no acknowledgement before the deadline, a transient
  state that does not settle in time, a reply of Unavailable,
  DeadlineExceeded or a transport error, a phase that holds itself, a
  unit with unfinished steps leaving the directory), and *unresumable*
  otherwise (NotFound, InvalidRequest, a started phase found Stopped,
  Aborted, Resetting or Idle without being told to, a phase exceeding its
  command budget). A resumable failure while Running or Restarting holds
  the procedure; an unresumable one while Running, Restarting, Holding or
  Held aborts it; otherwise a failure is only reported. A rejected command
  only updates the known state.
- **Timeouts.** Every command carries the acknowledgement timeout, which
  is also the request's timeout and so reaches the unit as its deadline.
  A transient state (Holding, Restarting, Stopping, Aborting, Resetting)
  has a settle timeout. A phase whose deadline passes becomes
  *unreachable*: its state is unknown, it no longer blocks Holding,
  Stopping or Aborting from finishing, and it blocks Restart until a
  report makes it known again. Running has no timeout.
- **Unit loss.** When the directory loses a unit (goodbye or TTL expiry),
  its phases become unreachable and nothing more is sent to it. If any of
  its steps is unfinished the procedure holds; Restart is refused while
  the unit is offline, and after it rejoins until its heartbeat reports
  its phases. A rejoined unit whose phase is still running is told to
  Hold, and Restart then resumes the batch. Abort ends a held batch
  Aborted; a lost phase's state is reported unknown. A unit whose steps
  are all done is only reported.
- **Bounds.** One input runs a fixed number of settle passes, each
  transition fires at most once per run, each phase gets at most a fixed
  number of commands per run, and every wait ends by a report, an
  acknowledgement, a deadline or the operator. So after Abort and the two
  timeouts every run is in a final state.

#### Wire contract

A unit serves each equipment phase on the request key
`unit/<node>/phase/<em>.<phase>` and publishes the phase's state on
`unit/<node>/phase/<em>.<phase>/state`, where `<node>` is the 8-byte node
id (the middleware `SenderId`) in lowercase hex. The engine's shell sends
each command with `Requester::request_with`, a timeout of the
acknowledgement deadline (which transports carry to the unit as
`RequestMeta::deadline_ms`) and `Target::Node` of the unit's node, so a
backend that can address nodes asks no one else; the key names the node
too, so key-routed backends reach the same unit. A unit's `HandlerError`
maps to the engine's acknowledgement errors (NotFound, InvalidRequest,
Unavailable, DeadlineExceeded; anything else is a transport error). Units
announce themselves in discovery as service kind `s88/unit` with the unit
id as instance and the unit class in TXT, which feeds the directory events
the engine sees as unit loss and return.

Bodies are fixed binary, version 1, integers little-endian: a command is
the version, the command code, a 32-bit id, a count and that many 32-bit
arguments (only Start carries arguments); an acknowledgement is the
version, accepted or rejected, the phase state code and the id; a state
report is the version, the state code, the id of the last applied command,
a count and that many 32-bit result values (a Measure phase's reading).
A unit publishes a report on every state change and as a heartbeat. The
unit side of a command is a pure function: decode, check Start's arguments
against the phase's ranges (InvalidRequest otherwise), step the phase
state model and answer. Payloads serve the request keys through ABI 5
(`serve`, `on_request`, `node_id`; section "Serving requests"), and a
report naming a command's id still acknowledges it, for a unit that can
only publish.

#### Phase flows and the generated phase server

A unit's phase logic is written as flows. A flow whose entry is `phase
<em>.<phase> <state>` (an acting state, optionally `every N ms`) is that
phase state's body: it runs when the phase enters the state and, with a
period, again every period while the phase stays there. The action
`phase done` finishes the acting state, at once or after the body has run
a given number of times (a constant or a Start parameter); `phase report`
stores up to four result values (constants, pin readings, Start
parameters, the run count) for the phase's state reports. The flow check
(`state-table` `flow::check_flows_in`, with the unit's phases) requires a
phase of the unit, an acting state, one body flow per phase state, known
parameters, run counts only in the phase's own body and report pins that
are not outputs; the plant check requires each phase's `bodies` to name
flows whose entry is exactly that phase state.

For a unit with equipment phases the generator emits a phase server
(`web/src/blocks/phaseGen.ts`): at init it reads `node_id`, writes the
node's hex digits into each phase's keys, serves each request key and
publishes Idle; a request is decoded exactly as `wire::decode_command`,
Start's arguments are checked against the phase's ranges, and a bounded
switch generated from the phase state table answers accepted with the new
state or rejected with the current one, then enters the new state
(clearing the run count, logging it, publishing the report and running
its body; an acting state without a body is done at once). Done is a
pending flag that a bounded settle pass at the end of every export
applies through the same table, so bodies never call back into the state
machine. Reports go out on every state change, on `phase report` and
every second.

#### IR version 4 and migration

The saved document becomes a *plant*: `physical` (units, each with its
control modules, equipment modules and payload logic), `procedural`
(phase classes and equipment phases) and `recipes` (master recipes). A
unit's payload is exactly the version-3 model restricted to that unit, and
that is what the generator compiles. A version 1-3 payload migrates to a
plant of one unbound unit of class "Unit" with no equipment modules, no
phases and no recipes; its logic is kept as the unit's basic control, and
the unit's payload is the original model, so its generated code is
unchanged byte for byte. It is deliberately not wrapped in an "app" phase:
a phase runs only when started, so wrapping would change what an existing
payload does. The version-4 reader accepts every older file.

#### Requirements

- [x] Q1 The phase state model is a pure transition function over 11
  states, 6 commands and Done, exactly the table above (empty cells reject
  and keep the state); Abort is accepted wherever something is still
  happening; final states reset to Idle through Resetting; every state is
  reachable from Idle and returns to it; Done alone ends any acting state
  within three steps — `cargo test -p recipe-core phase`: "every_cell_of_the_command_matrix_matches_the_table", "acting_states_and_only_they_accept_done", "abort_is_accepted_wherever_something_is_still_happening", "final_states_reset_to_idle_through_resetting", "every_state_is_reachable_from_idle_and_every_state_reaches_idle", "done_alone_from_any_acting_state_ends_in_a_quiescent_state_within_three_steps".
- [x] Q2 States and commands have stable wire codes (0..10, 1..6) and
  names, mirrored by the web model and exposed to the page as the phase
  model — `cargo test -p recipe-core phase`: "codes_round_trip_and_unknown_codes_are_none"; `cargo test -p protocol-wasm recipes`: "the_phase_model_lists_states_commands_and_the_matrix"; `web/test/plant.test.ts`: "mirrors the Rust phase model's names"; `web/test/recipe.check.test.ts`: "exposes the phase state model matching the page's names".
- [x] Q3 Master recipes are checked by the rules above (identifiers unique
  ignoring case; known unit procedures and classes; every parameter given
  once and in range; known steps, non-empty sides, no repeats; a step in at
  most one transition per side; no cycle; limits; unused unit procedures
  warn) and compile to a bounded chart with initial steps, transition
  masks and a "done before" relation for concurrency — `cargo test -p recipe-core recipe`: "the_worked_example_has_no_issues_and_compiles_to_a_two_step_chain", "ids_must_be_identifiers_unique_ignoring_case", "class_rules_cover_names_parameter_count_and_ranges", "a_step_needs_a_known_unit_procedure_and_class", "arguments_name_class_parameters_once_cover_all_and_lie_in_range", "transitions_need_both_sides_known_steps_and_no_repeats", "a_step_ends_at_most_one_transition_and_starts_from_at_most_one", "cycles_are_errors_on_each_of_their_transitions", "a_convergence_waiting_on_its_own_successor_is_a_cycle", "parallel_divergence_and_convergence_compile_with_concurrency", "a_recipe_needs_steps_and_respects_its_limits", "an_unused_unit_procedure_is_only_a_warning", "compile_refuses_a_recipe_with_errors".
- [x] Q4 Allocation follows the rules above: candidates by presence, unit
  class, phase class and ranges; exclusive units found by a bounded
  backtracking search; ties by unit id, then module and phase name;
  concurrent steps never share a phase; failures name the procedure and
  each unit's reason, or contention — `cargo test -p recipe-core alloc`: "the_worked_example_binds_blink_to_the_led_unit_and_measure_to_the_sensor", "the_result_does_not_depend_on_the_order_units_are_listed_in", "an_offline_or_wrong_class_unit_is_rejected_with_its_reason", "a_unit_without_the_class_or_with_a_narrower_range_is_rejected", "two_procedures_cannot_share_one_unit", "the_search_backtracks_to_find_an_exclusive_assignment", "ties_go_to_the_smallest_unit_id_and_the_smallest_module_and_phase", "concurrent_steps_take_different_phases_and_sequential_ones_may_share", "too_many_units_is_refused".
- [x] Q5 The engine runs steps and transitions as above with
  level-triggered commands (at most one outstanding per phase), releases
  done phases with Reset, completes only when every phase is back to Idle,
  accepts a report naming a command's id as its acknowledgement, ignores
  stale reports, and moves the procedure by the phase state model
  (operator commands outside it are refused) — `cargo test -p recipe-core engine`: "the_worked_example_runs_blink_then_measure_and_releases_both_phases", "a_report_naming_the_command_acknowledges_it", "reports_older_than_the_last_applied_command_are_ignored", "operator_commands_follow_the_phase_table", "hold_holds_the_running_phase_and_restart_resumes_it", "stop_and_abort_end_the_run_in_stopped_and_aborted", "a_rejected_command_updates_the_known_state_and_is_retried".
- [x] Q6 Failures follow the policy above: resumable ones hold, unresumable
  ones abort, rejections only update the state, and a phase over its
  command budget aborts — `cargo test -p recipe-core engine`: "a_missing_acknowledgement_holds_the_recipe_and_makes_the_phase_unknown", "not_found_and_invalid_request_abort_while_unavailable_holds", "a_phase_that_holds_itself_holds_the_recipe_and_one_that_aborts_aborts_it", "a_rejected_command_updates_the_known_state_and_is_retried", "a_phase_that_keeps_rejecting_exhausts_its_budget_and_aborts".
- [x] Q7 Losing a unit with unfinished steps holds the batch, nothing is
  sent to it, Restart is refused while it is offline and until its
  heartbeat makes its phases known, a rejoined unit's running phase is
  held and Restart then completes the batch, Abort ends it Aborted; losing
  a unit whose steps are done is only reported — `cargo test -p recipe-core engine`: "losing_a_unit_with_unfinished_steps_holds_and_restart_waits_for_it", "losing_a_unit_whose_steps_are_done_is_only_reported"; `cargo test -p recipe-core sim`: "losing_the_led_unit_holds_the_batch_and_abort_ends_it_aborted", "a_lost_unit_that_comes_back_is_held_and_the_batch_restarts_to_complete".
- [x] Q8 Timeouts are deadlines: every command carries the acknowledgement
  timeout, which reaches the unit as `RequestMeta::deadline_ms`; a missed
  acknowledgement or an unsettled transient state makes the phase
  unreachable and holds the batch — `cargo test -p recipe-core engine`: "a_missing_acknowledgement_holds_the_recipe_and_makes_the_phase_unknown", "a_transient_state_that_never_settles_times_out"; `cargo test -p recipe-core sim`: "a_phase_stuck_holding_times_out_and_the_batch_is_held_with_it_unknown"; `cargo test --test recipe_wire`: "the_worked_example_runs_to_complete_over_request_with_and_pub_sub".
- [x] Q9 Termination: for 3000 random interleavings of operator commands,
  unit losses and returns and stuck phases on the worked example, followed
  by Abort, every run is Complete, Stopped, Aborted (or Idle, never
  started or reset) within the acknowledgement plus settle timeouts, no
  input overflows its effects, no command goes to a unit the engine was
  told is offline, and every ending and the hold path occur; without
  failures every ordering of unit speeds completes — `cargo test -p recipe-core sim`: "every_run_ends_complete_stopped_or_aborted_after_abort_and_two_timeouts", "without_failures_every_ordering_of_unit_speeds_completes".
- [x] Q10 The wire contract is as above: keys with the node in hex that
  parse back, version-1 command, acknowledgement and report bodies that
  round-trip and refuse malformed input, and a pure unit side that checks
  Start's arguments against the phase's ranges; over the middleware 0.5
  traits (memory backend) the engine's commands go out with
  `request_with`, a timeout and `Target::Node`, units answer through a
  `Handler`, reports come back over pub/sub, the worked example
  completes, a command addressed to another node fails and holds, and an
  out-of-range Start is InvalidRequest — `cargo test -p recipe-core wire`: "keys_name_the_node_in_hex_and_parse_back", "malformed_keys_and_identifiers_are_refused", "command_ack_and_report_bodies_round_trip", "malformed_bodies_are_refused", "serve_applies_the_table_and_checks_start_arguments_against_ranges"; `cargo test --test recipe_wire`: "the_worked_example_runs_to_complete_over_request_with_and_pub_sub", "a_command_addressed_to_another_node_fails_and_holds_the_batch", "a_start_outside_the_phase_range_is_an_invalid_request_that_aborts".
- [x] Q11 `recipe-core` is no_std with no heap, no `unsafe` and no
  recursion, and builds for `thumbv6m-none-eabi` — `scripts/check-features.sh` (the Q11 build of `recipe-core` and `state-table` for `thumbv6m-none-eabi`).
- [x] Q12 The plant document (IR version 4) splits the model into
  physical, procedural and recipes as above, with a strict shape and
  reference check naming the place (units, node ids, control and
  equipment modules, classes, phase ranges for every class parameter
  inside the class's, body flows of the same unit used once, recipes) and
  a JSON round trip that copies — `web/test/plant.test.ts`: "the worked example is a valid plant and round-trips through JSON as a copy", "rejects bad shapes and dangling references, naming the place", "each unit's payload is its own board, control modules, logic and machines", "lists allocation units with ranges in class parameter order", "a payload with no logic beyond its own still makes a plant of one unit".
- [x] Q13 Version 1-3 payloads and legacy Blocks exports migrate to a
  one-unit plant whose unit payload equals the original model and
  generates the same code byte for byte, also after saving and reading
  the plant — `web/test/plant.test.ts`: "migrates version-3 payloads to one unit whose payload generates the same code byte for byte", "migrates version-1 and -2 files and legacy Blocks exports through the version-3 reader".
- [x] Q14 The page reaches the rules through protocol-wasm: `checkRecipe`
  (plain-language messages by place), `allocateRecipe` (bindings, or per
  unit reasons), `simulateRecipe` (a timed trace against simulated units
  following a script of operator commands and unit losses) and
  `phaseModel` — `cargo test -p protocol-wasm recipes`: "the_worked_example_checks_clean_and_binds_each_step", "check_messages_name_the_step_and_the_rule", "allocation_failures_say_per_unit_why", "a_simulated_run_completes_and_a_hold_and_a_unit_loss_show_in_the_trace"; `web/test/recipe.check.test.ts`: "the worked example checks clean and allocates Blink to the LED unit, Measure to the sensor unit", "reports recipe errors in plain language, by place", "an allocation failure says, per unit, why it cannot host the procedure", "an unknown script kind throws".
- [x] Q15 The worked example "Blink then Measure" (classes Blink and
  Measure; a Pico 2 unit with an LED module and an ESP32 unit with an ADC
  module) allocates Blink to the LED unit and Measure to the sensor unit
  and runs to Complete with Measure started when Blink completes; a Hold
  mid-run is Held after the settle time, starts nothing new, and Restart
  completes the batch; losing the LED unit holds the batch, Restart is
  refused, and Abort ends it Aborted — `cargo test -p recipe-core sim`: "the_worked_example_runs_to_complete", "a_hold_mid_run_holds_blink_and_a_restart_finishes_the_batch", "losing_the_led_unit_holds_the_batch_and_abort_ends_it_aborted"; `web/test/recipe.check.test.ts`: "runs to Complete against simulated units: Blink first, Measure when Blink completes", "a Hold mid-run holds the batch until Restart, and losing a unit holds it until Abort ends it Aborted".
- [ ] Q16 Physical and procedural editors: a Units view (unit list, unit
  class, node binding from `sys/info`), the Pins table as each unit's
  control-module table, equipment modules as a table grouping its rows,
  and a phase table per module (class, ranges, a body flow per acting
  state) with the phase state matrix shown read-only; the editor saves
  version 4.
- [x] Q17 Unit payload code generation exposes the phase API: a flow entry
  "phase <em>.<phase> enters <state>" and a "phase done" action, generated
  phase tables answering the wire contract (an ABI request export for
  payloads, or subscribe-and-report until it exists), heartbeat reports,
  and argument range checks, all checked against the phase state model —
  `cargo test -p state-table flow`: "a_blink_body_with_done_after_a_parameter_and_a_report_is_clean", "phase_entries_name_a_phase_of_the_unit_and_an_acting_state", "a_phase_state_has_one_body_flow", "phase_actions_name_a_phase_and_its_parameters_and_count_only_in_its_body", "a_report_carries_at_most_four_values"; `cargo test -p recipe-core phase`: "the_flow_checks_acting_states_are_these_acting_states_in_lower_case"; `cargo test -p protocol-wasm flows`: "a_phase_body_is_clean_and_phase_problems_read_plainly"; `web/test/phase.gen.test.ts`: "switches on the phase model's table", "leaves a unit without phases exactly as before", "generates a bounded command switch, serve per phase, reports and the heartbeat", "reports phase actions on unknown phases, parameters and run counts outside the body", "the worked example's units check clean against their phases", "compiles the payload-sim fixtures: the 77-cell matrix unit and the worked example's two units"; `web/test/plant.test.ts`: "rejects bad shapes and dangling references, naming the place"; `cargo test -p payload-sim --test phase`: "p1_every_cell_of_the_phase_model_matches_recipe_core" (all 77 cells, acknowledgements and reports byte-equal to `recipe_core::wire`), "p2_start_arguments_and_malformed_bodies_are_invalid_requests", "p3_blink_then_measure_runs_to_complete_on_the_generated_units", "p4_a_hold_mid_blink_holds_the_batch_and_restart_finishes_it", "p5_losing_the_led_unit_holds_restart_is_refused_and_abort_ends_aborted".
- [ ] Q18 The master recipe is edited on the Flow canvas: steps as nodes
  naming a phase class and arguments, transitions as bars joining steps
  (parallel divergence and convergence), unit procedures as lanes, with
  `checkRecipe` issues on their nodes.
- [ ] Q19 Allocation and execution in the browser: an allocation panel
  against units from discovery or simulated units, and the engine running
  the control recipe with operator buttons (Start, Hold, Restart, Stop,
  Abort, Reset), live step and phase states, and the trace.
- [ ] Q20 Selection (alternative) divergence: transitions with conditions
  on phase result values, keeping the termination argument.

## Known limitations (0.3)

- **Publish and receive share one `&mut self`.** A transport that owns both directions of one radio cannot publish while a receive is pending. Callers serialise them or keep per-direction tasks behind channels. Split halves are deferred.
- **One borrowed subscriber at a time on single-stream transports.** Since 0.3 `Subscribe::Subscriber<'a>` may borrow the transport (serial C12, ESP-NOW N13, BLE advertising A13), so only one such view exists at once; independent concurrent subscriptions still need a multiplexing transport (Zenoh, gRPC bus) or the runtime filter slots. The GATT client (`BleClient`) does not implement `Subscribe` yet.
- **`SampleMeta.seq` is 8 bits** (documented on the field): it distinguishes repeats inside a dedup window, not order in a long stream; compare it with wrapping arithmetic.

Lifted in 0.3: owned keys (`KeyBuf<N>`, `KeyFilterBuf<N>`), runtime `Subscribe` on single-stream transports, and publisher identity on the gRPC bus (G8).
