;; SC7 on the EVKB: streams the FXOS8700CQ. init starts an IMU route
;; (route_source kind 2: 50 Hz, ±2 g, 3 channels, 10 samples per batch, so a
;; batch every 200 ms) on imu/raw and publishes route_source's result (the
;; handle, i32 LE) on imu/started. Every second the tick publishes the
;; route's counters on imu/stat: Frames (batches published), FramesDropped
;; (samples the sensor overwrote) and ChunksDropped (failed reads and
;; refused batches), i32 LE each.
;;
;;   wat2wasm imu.wat -o imu.wasm
(module
  (import "mw" "log"          (func $log (param i32 i32)))
  (import "mw" "publish"      (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "set_tick"     (func $set_tick (param i32) (result i32)))
  (import "mw" "route_source" (func $route_source (param i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_stat"   (func $route_stat (param i32 i32) (result i32)))
  (memory (export "memory") 1 1)
  (data (i32.const 16) "imu/raw")       ;; 7
  (data (i32.const 32) "imu/started")   ;; 11
  (data (i32.const 48) "imu/stat")      ;; 8
  (data (i32.const 64) "imu: started")  ;; 12
  ;; ImuConfig: rate 50 (u16), range 2, channels 3, samples 10 (u16), flags 0.
  (data (i32.const 80) "\32\00\02\03\0a\00\00\00")
  (global $handle (mut i32) (i32.const -1))
  (func (export "init")
    (global.set $handle
      (call $route_source (i32.const 2) (i32.const 16) (i32.const 7) (i32.const 80) (i32.const 8)))
    (i32.store (i32.const 96) (global.get $handle))
    (drop (call $publish (i32.const 32) (i32.const 11) (i32.const 96) (i32.const 4)))
    (drop (call $set_tick (i32.const 1000)))
    (call $log (i32.const 64) (i32.const 12)))
  (func (export "tick") (param $now_ms i32)
    (i32.store (i32.const 112) (call $route_stat (global.get $handle) (i32.const 0)))
    (i32.store (i32.const 116) (call $route_stat (global.get $handle) (i32.const 1)))
    (i32.store (i32.const 120) (call $route_stat (global.get $handle) (i32.const 3)))
    (drop (call $publish (i32.const 48) (i32.const 8) (i32.const 112) (i32.const 12))))
)
