;; SC8 on the EVKB: microphone levels. init starts a microphone route
;; (route_source kind 3: 16 kHz, gain 0 dB, mode 0 = levels, 5 blocks per
;; batch, 800-sample blocks, so 20 levels a second in a batch every 250 ms)
;; on mic/level and publishes route_source's result (the handle, i32 LE) on
;; mic/started. Every second the tick publishes the route's counters on
;; mic/stat: Frames (batches published), FramesDropped (samples not
;; published) and ChunksDropped (refused batches), i32 LE each.
;;
;;   wat2wasm mic_levels.wat -o mic_levels.wasm
(module
  (import "mw" "log"          (func $log (param i32 i32)))
  (import "mw" "publish"      (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "set_tick"     (func $set_tick (param i32) (result i32)))
  (import "mw" "route_source" (func $route_source (param i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_stat"   (func $route_stat (param i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (data (i32.const 16) "mic/level")     ;; 9
  (data (i32.const 32) "mic/started")   ;; 11
  (data (i32.const 48) "mic/stat")      ;; 8
  (data (i32.const 64) "mic: levels")   ;; 11
  ;; MicConfig: rate 16000 (u16), gain 0, mode 0, units 5 (u16), block 800 (u16).
  (data (i32.const 80) "\80\3e\00\00\05\00\20\03")
  (global $handle (mut i32) (i32.const -1))
  (func (export "init")
    (global.set $handle
      (call $route_source (i32.const 3) (i32.const 16) (i32.const 9) (i32.const 80) (i32.const 8)))
    (i32.store (i32.const 96) (global.get $handle))
    (drop (call $publish (i32.const 32) (i32.const 11) (i32.const 96) (i32.const 4)))
    (drop (call $set_tick (i32.const 1000)))
    (call $log (i32.const 64) (i32.const 11)))
  (func (export "tick") (param $now_ms i32)
    (i32.store (i32.const 112) (call $route_stat (global.get $handle) (i32.const 0)))
    (i32.store (i32.const 116) (call $route_stat (global.get $handle) (i32.const 1)))
    (i32.store (i32.const 120) (call $route_stat (global.get $handle) (i32.const 3)))
    (drop (call $publish (i32.const 48) (i32.const 8) (i32.const 112) (i32.const 12))))
)
