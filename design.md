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

### Feature isolation

- [x] F1 `ble` and `postcard` build for `thumbv6m-none-eabi` without `std` —
  `scripts/check-features.sh` (builds `ble`, `ble,postcard`, `postcard` for `thumbv6m-none-eabi`, asserts `std` off).
- [x] F2 `zenoh`, `grpc` and `istio` build on the host without pulling in the
  `ble` module —
  `scripts/check-features.sh` (builds `zenoh`, `grpc`, `grpc,istio`, `zenoh,grpc,istio,prost`, asserts `ble` off).
- [x] F3 Enabling a cloud feature never enables an embedded feature and vice
  versa (checked by `scripts/check-features.sh` via `cargo tree`) —
  `scripts/check-features.sh` (`cargo tree` shows no zenoh/tonic/tokio/hyper/h2 in embedded sets, no `ble` in cloud sets).

### BLE GATT (`ble`)

- [x] B1 Keys map to stable, distinct 128-bit characteristic UUIDs and 16-bit
  advertising ids in a fixed namespace — `ble::uuid::tests::*`.
- [x] B2 Messages larger than one ATT payload are fragmented and reassembled;
  out-of-order, duplicate or oversized fragments are rejected —
  `ble::frame::tests::*` (`large_message_is_fragmented_and_reassembled`, `out_of_order_fragment_is_rejected`, `duplicate_fragment_is_rejected`, `oversized_message_is_rejected`, `interleaved_and_inconsistent_fragments_are_rejected`, `malformed_fragments_are_rejected`), `ble::gatt::tests::responder_rejects_duplicate_fragment`, `responder_rejects_oversized_request_and_tells_requester`.
- [x] B3 Publish sends a notification on the key's characteristic; subscribe
  yields notifications received on it —
  `ble::gatt::tests::publish_notifies_fragments_on_key_characteristic`, `subscribe_yields_notification_for_subscribed_key`, `subscribe_reports_small_buffer_and_bad_fragments`, `end_to_end_publish_subscribe_over_in_memory_link`.
- [x] B4 Requests carry a correlation id; the response matching it is
  returned and stale responses are discarded. Handler errors reach the caller —
  `ble::gatt::tests::request_carries_correlation_id_and_discards_stale_responses`, `successive_requests_use_new_correlation_ids`, `handler_error_reaches_requester`, `responder_maps_failures_to_status_codes`, `end_to_end_request_over_in_memory_link`.
- [x] B5 The radio is reached only through traits, so a host test drives the
  transport with an in-memory fake —
  all `ble::gatt::tests::*` drive `BleTransport` through the in-memory `FakeLink`; `att_mtu_is_range_checked`.

### BLE advertising (`ble`)

- [x] A1 A builder produces legacy advertising data (≤31 bytes): flags,
  complete or shortened local name, middleware service UUID, and manufacturer
  data. Data that does not fit is an error —
  `ble::advert::tests::builder_emits_all_fields`, `builder_uses_configured_company_id`, `long_name_is_shortened_or_rejected`, `oversized_fields_are_an_error`.
- [x] A2 Advertising data is parsed into AD structures; malformed lengths are
  rejected without panicking —
  `ble::advert::tests::parser_rejects_malformed_lengths`, `parser_stops_at_zero_padding`.
- [x] A3 Connectionless publish: a payload on a key is broadcast as
  manufacturer data `[short id][sequence][payload]` —
  `ble::advert::tests::publish_broadcasts_short_id_sequence_and_payload`, `publish_rejects_payload_that_does_not_fit`.
- [x] A4 Scanning subscriber yields samples whose short id matches a
  subscribed key and drops repeated sequence numbers —
  `ble::advert::tests::scanning_subscriber_matches_keys_and_drops_repeats`, `dedup_table_tracks_each_slot`, `scanning_subscriber_errors`, `end_to_end_publish_to_scanning_subscriber`.
- [x] A5 Discovery: a scan report advertising the middleware service UUID is
  recognised as a middleware node, with its name —
  `ble::advert::tests::discovery_recognises_middleware_nodes`.

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