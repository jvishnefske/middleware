;; ABI 3 PWM and ADC fixture (X15), shared by the protocol-wasm Rust tests
;; and web/test/abi3.test.ts. init opens PWM timer 0 at 1 kHz and ticks
;; every 10 ms. The first two ticks read ADC pin 4, publish the reading on
;; app/adc (i32 LE) and set timer 0 channel 1 to reading * 16. The third
;; tick stops the timer and publishes the result of a pwm_set on the
;; stopped timer (Busy, -7) on app/err.
(module
  (import "mw" "pwm_open" (func $pwm_open (param i32 i32) (result i32)))
  (import "mw" "pwm_set" (func $pwm_set (param i32 i32 i32) (result i32)))
  (import "mw" "pwm_stop" (func $pwm_stop (param i32) (result i32)))
  (import "mw" "adc_read" (func $adc_read (param i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "set_tick" (func $set_tick (param i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "app/adc")
  (data (i32.const 16) "app/err")
  (global $n (mut i32) (i32.const 0))
  (func (export "init")
    (drop (call $pwm_open (i32.const 0) (i32.const 1000)))
    (drop (call $set_tick (i32.const 10))))
  (func (export "tick") (param i32)
    (local $v i32)
    (global.set $n (i32.add (global.get $n) (i32.const 1)))
    (if (i32.eq (global.get $n) (i32.const 3))
      (then
        (drop (call $pwm_stop (i32.const 0)))
        (i32.store (i32.const 64) (call $pwm_set (i32.const 0) (i32.const 0) (i32.const 1)))
        (drop (call $publish (i32.const 16) (i32.const 7) (i32.const 64) (i32.const 4)))
        (return)))
    (if (i32.gt_s (global.get $n) (i32.const 3)) (then (return)))
    (local.set $v (call $adc_read (i32.const 4)))
    (i32.store (i32.const 64) (local.get $v))
    (drop (call $publish (i32.const 0) (i32.const 7) (i32.const 64) (i32.const 4)))
    (drop (call $pwm_set (i32.const 0) (i32.const 1) (i32.mul (local.get $v) (i32.const 16))))))
