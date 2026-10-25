;; Button template (hand-written WAT for the middleware payload ABI 2).
;;
;; A falling edge on GPIO 0 (the BOOT button of ESP32-S3 dev boards, input
;; with pull-up) toggles the LED on GPIO 8 and publishes the press count on
;; "app/button/count" as ASCII decimal. Presses within 20 ms of the last
;; accepted one are contact bounce and are ignored.
;;
;; Interrupt model: the firmware's ISR only timestamps the edge and queues
;; it; the main loop later calls the exported on_irq(kind, source, value,
;; time_us) with kind 1 (GPIO), source = pin, value = level after the edge
;; and the ISR's microsecond timestamp. This code never runs inside an ISR.
(module
  (import "mw" "log"         (func $log (param i32 i32)))
  (import "mw" "publish"     (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "gpio_input"  (func $gpio_input (param i32 i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write"  (func $gpio_write (param i32 i32) (result i32)))
  (import "mw" "gpio_irq"    (func $gpio_irq (param i32 i32) (result i32)))

  (memory (export "memory") 1 1)

  (data (i32.const 16) "app/button/count")       ;; 16 bytes
  (data (i32.const 32) "button: started")        ;; 15 bytes
  (data (i32.const 48) "button: gpio refused")   ;; 20 bytes
  ;; Offsets 96..106 hold the decimal digits being published.

  (global $button i32 (i32.const 0))
  (global $led_pin i32 (i32.const 8))
  (global $debounce_us i32 (i32.const 20000))
  (global $count (mut i32) (i32.const 0))
  (global $led (mut i32) (i32.const 0))
  (global $last_us (mut i32) (i32.const 0))
  (global $pressed_once (mut i32) (i32.const 0))

  ;; Writes $n in decimal so that it ends at offset 106; returns the start.
  (func $utoa (param $n i32) (result i32)
    (local $p i32)
    (local.set $p (i32.const 106))
    (loop $digit
      (local.set $p (i32.sub (local.get $p) (i32.const 1)))
      (i32.store8 (local.get $p)
        (i32.add (i32.const 48) (i32.rem_u (local.get $n) (i32.const 10))))
      (local.set $n (i32.div_u (local.get $n) (i32.const 10)))
      (br_if $digit (local.get $n)))
    (local.get $p))

  (func (export "init")
    ;; gpio_input(pin, pull = 1 up), then gpio_irq(pin, edge = 2 falling).
    (if (i32.or
          (i32.lt_s (call $gpio_input (global.get $button) (i32.const 1)) (i32.const 0))
          (i32.lt_s (call $gpio_irq (global.get $button) (i32.const 2)) (i32.const 0)))
      (then (call $log (i32.const 48) (i32.const 20))))
    (drop (call $gpio_output (global.get $led_pin)))
    (call $log (i32.const 32) (i32.const 15)))

  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $time_us i32)
    (local $start i32)
    ;; Only GPIO events (kind 1) for the button pin, falling to level 0.
    (if (i32.ne (local.get $kind) (i32.const 1)) (then (return)))
    (if (i32.ne (local.get $source) (global.get $button)) (then (return)))
    (if (local.get $value) (then (return)))
    ;; Debounce with wrapping time arithmetic.
    (if (i32.and (global.get $pressed_once)
          (i32.lt_u (i32.sub (local.get $time_us) (global.get $last_us))
                    (global.get $debounce_us)))
      (then (return)))
    (global.set $pressed_once (i32.const 1))
    (global.set $last_us (local.get $time_us))
    (global.set $count (i32.add (global.get $count) (i32.const 1)))
    (global.set $led (i32.xor (global.get $led) (i32.const 1)))
    (drop (call $gpio_write (global.get $led_pin) (global.get $led)))
    (local.set $start (call $utoa (global.get $count)))
    (drop (call $publish (i32.const 16) (i32.const 16)
      (local.get $start) (i32.sub (i32.const 106) (local.get $start)))))
)
