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
### mDNS service discovery (`mdns`)

`MdnsSocket` browses DNS-SD service types (`_service._tcp|_udp.domain`)
over mDNS; since 0.5 it does so as a `discovery::Browse` (L12), and the
0.4 `ServiceDiscovery` trait is gone. The message format is pure, always
built and lives in `dns::wire` since 0.5 (`mdns::wire` re-exports it); the
tokio socket is behind the `mdns` feature and queries as a one-shot querier
from an ephemeral port, so it coexists with a system mDNS daemon on port
5353. The D tests below kept their names when the codec moved.

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
  kept for the next call; `try_recv` is `None` when nothing has arrived; a
  malformed datagram is an error and is dropped —
  `mdns::socket::tests::try_recv_is_none_when_empty_and_keeps_an_oversized_response`,
  `mdns::socket::tests::malformed_datagrams_are_errors_then_dropped`.
- [x] D8 Every receive call is bounded in datagrams (`MAX_IGNORED_DATAGRAMS`)
  and a cancelled `recv` loses no response —
  `mdns::socket::tests::ignored_limit_bounds_try_recv`,
  `mdns::socket::tests::cancelled_recv_loses_no_response`.
- [x] D9 The pure core builds for `thumbv6m-none-eabi`; `mdns` enables `std`
  and no embedded feature — `scripts/check-features.sh`.
- [x] D10 Tests put no multicast traffic on the network: the socket is
  aimed at a loopback responder — `mdns::socket::tests::*`.

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
  backend declares them (serial, Zenoh: `Block`; ESP-NOW, BLE GATT, BLE
  advertising: `DropNewest`; gRPC, memory: `DropOldest`); the
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
- [ ] K11 `Split` divides a link into a `Publisher` half and a
  `Subscriber` half usable concurrently. (Trait defined in
  `transport::pubsub`; the embedded implementations and their tests are
  0.5 phase 2.)
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
- [ ] K15 *(phase 2)* Zenoh, gRPC, serial (`mwctl` fake) and the mDNS and
  unicast DNS-SD backends pass every conformance case for the traits they
  implement, with caller identity carried where the transport
  authenticates it (Istio SPIFFE principal on gRPC).

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
  `Down`, and an event too large for the buffer is kept —
  `mdns::socket::tests::browse_maps_the_kind_and_reports_up_then_down`,
  `mdns::socket::tests::browse_reports_an_unaddressed_instance_by_host`
  *(mdns)*.
- [x] L13 `typed::TypedBrowser` decodes a service's description from its
  TXT entry `m=<bytes>` with a codec (`typed::encode_txt` writes it) —
  `typed::tests::typed_browser_decodes_txt_descriptions`.
- [ ] L14 *(phase 2)* An mDNS announcer (`Announce` on `MdnsSocket`) and
  a unicast DNS-SD resolver (`dns::unicast`, feature `dns-sd`) pass the
  discovery conformance cases on loopback.
- [x] L15 `dns-sd` enables `std` and no embedded feature, and no embedded
  feature set enables it (or any host feature) —
  `scripts/check-features.sh`.

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
- [x] E6 Produces merged flash images per chip for the web flasher —
  `firmware/build-images.sh` writes `web/public/firmware/{esp32,esp32s3,esp32c3}.bin` and `manifest.json`.
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

### Host tool (`tools/mwctl`)

`mwctl --port <PATH> info|upload|start|stop|monitor|reset` drives a device
over one named serial port, synchronously, with the same pure frame and
loader code as the firmware. It never enumerates ports. It opens the port
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
  `npm test` in `web/` (115 tests; `web/check.sh` runs it and the build) and `cargo test -p protocol-wasm` (17 tests).
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
    camera port; other chips none (`Unsupported`); PWM, I2C and camera
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

## Known limitations (0.3)

- **Publish and receive share one `&mut self`.** A transport that owns both directions of one radio cannot publish while a receive is pending. Callers serialise them or keep per-direction tasks behind channels. Split halves are deferred.
- **One borrowed subscriber at a time on single-stream transports.** Since 0.3 `Subscribe::Subscriber<'a>` may borrow the transport (serial C12, ESP-NOW N13, BLE advertising A13), so only one such view exists at once; independent concurrent subscriptions still need a multiplexing transport (Zenoh, gRPC bus) or the runtime filter slots. The GATT client (`BleClient`) does not implement `Subscribe` yet.
- **`SampleMeta.seq` is 8 bits** (documented on the field): it distinguishes repeats inside a dedup window, not order in a long stream; compare it with wrapping arithmetic.

Lifted in 0.3: owned keys (`KeyBuf<N>`, `KeyFilterBuf<N>`), runtime `Subscribe` on single-stream transports, and publisher identity on the gRPC bus (G8).
