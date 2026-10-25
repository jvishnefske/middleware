;; Blink template (hand-written WAT for the middleware payload ABI v1).
;;
;; Toggles GPIO 8 every 500 ms and publishes the level on "app/blink/state"
;; as one ASCII byte, "1" or "0". Every import lives in module "mw" and uses
;; only i32 values; pointers are offsets into the exported memory, and a
;; negative result is an error code (-1 invalid argument ... -6 invalid key).
(module
  (import "mw" "log"         (func $log (param i32 i32)))
  (import "mw" "publish"     (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "set_tick"    (func $set_tick (param i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write"  (func $gpio_write (param i32 i32) (result i32)))

  (memory (export "memory") 1 1)

  ;; Constant strings: offset, then length used below.
  (data (i32.const 16) "app/blink/state")        ;; 15 bytes
  (data (i32.const 32) "blink: started")         ;; 14 bytes
  (data (i32.const 48) "blink: gpio refused")    ;; 19 bytes
  ;; Offset 80 holds the one-byte message being published.

  (global $pin i32 (i32.const 8))
  (global $level (mut i32) (i32.const 0))

  (func (export "init")
    ;; A refused pin is logged; the payload keeps publishing anyway.
    (if (i32.lt_s (call $gpio_output (global.get $pin)) (i32.const 0))
      (then (call $log (i32.const 48) (i32.const 19))))
    (drop (call $set_tick (i32.const 500)))
    (call $log (i32.const 32) (i32.const 14)))

  (func (export "tick") (param $now_ms i32)
    (global.set $level (i32.xor (global.get $level) (i32.const 1)))
    (drop (call $gpio_write (global.get $pin) (global.get $level)))
    ;; '0' + level
    (i32.store8 (i32.const 80) (i32.add (i32.const 48) (global.get $level)))
    (drop (call $publish (i32.const 16) (i32.const 15) (i32.const 80) (i32.const 1))))
)
