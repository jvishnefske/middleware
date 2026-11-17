;; SC6 fixture: streams the fake IMU and the fake microphone. init starts
;; an IMU route (kind 2: 50 Hz, ±2 g, 3 channels, 5 samples per batch) on
;; app/imu and a microphone route (kind 3: 8 kHz, levels of 400-sample
;; blocks, 2 blocks per batch) on app/mic, and publishes both route_source
;; results (i32 LE each) on app/started; both routes publish a batch every
;; 100 ms. on_irq counts RouteFrames events (kind 5); at the sixth it stops
;; route 0 (the IMU) and publishes route_stop's result on app/stopped.
(module
  (import "mw" "route_source" (func $route_source (param i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "route_stop" (func $route_stop (param i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "app/imu")
  (data (i32.const 32) "app/mic")
  (data (i32.const 48) "app/started")
  (data (i32.const 96) "app/stopped")
  ;; ImuConfig: rate 50 (u16), range 2, channels 3, samples 5 (u16), flags 0.
  (data (i32.const 64) "\32\00\02\03\05\00\00\00")
  ;; MicConfig: rate 8000 (u16), gain 0, mode 0 (levels), units 2 (u16), block 400 (u16).
  (data (i32.const 72) "\40\1f\00\00\02\00\90\01")
  (global $frames (mut i32) (i32.const 0))
  (func (export "init")
    (i32.store (i32.const 0)
      (call $route_source (i32.const 2) (i32.const 16) (i32.const 7) (i32.const 64) (i32.const 8)))
    (i32.store (i32.const 4)
      (call $route_source (i32.const 3) (i32.const 32) (i32.const 7) (i32.const 72) (i32.const 8)))
    (drop (call $publish (i32.const 48) (i32.const 11) (i32.const 0) (i32.const 8))))
  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $t i32)
    (if (i32.eq (local.get $kind) (i32.const 5))
      (then
        (global.set $frames (i32.add (global.get $frames) (i32.const 1)))
        (if (i32.eq (global.get $frames) (i32.const 6))
          (then
            (i32.store (i32.const 8) (call $route_stop (i32.const 0)))
            (drop (call $publish (i32.const 96) (i32.const 11) (i32.const 8) (i32.const 4)))))))))
