;; Timer template (hand-written WAT for the middleware payload ABI 2).
;;
;; Starts hardware timer 0 with a 500 ms period. Each alarm toggles the LED
;; on GPIO 8 and publishes the total number of periods on "app/timer/count"
;; as ASCII decimal.
;;
;; Interrupt model: the alarm ISR only queues the event; the main loop calls
;; on_irq(kind = 2, source = timer id, value = periods, time_us). If alarms
;; fire while one is still queued they coalesce into a single event whose
;; value counts them, so value can exceed 1 and the count stays exact.
(module
  (import "mw" "log"         (func $log (param i32 i32)))
  (import "mw" "publish"     (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write"  (func $gpio_write (param i32 i32) (result i32)))
  (import "mw" "timer_start" (func $timer_start (param i32 i32 i32) (result i32)))

  (memory (export "memory") 1 1)

  (data (i32.const 16) "app/timer/count")        ;; 15 bytes
  (data (i32.const 32) "timer: started")         ;; 14 bytes
  (data (i32.const 48) "timer: refused")         ;; 14 bytes
  ;; Offsets 96..106 hold the decimal digits being published.

  (global $timer i32 (i32.const 0))
  (global $led_pin i32 (i32.const 8))
  (global $count (mut i32) (i32.const 0))
  (global $led (mut i32) (i32.const 0))

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
    (drop (call $gpio_output (global.get $led_pin)))
    ;; timer_start(id, period_us, periodic = 1)
    (if (i32.lt_s (call $timer_start (global.get $timer) (i32.const 500000) (i32.const 1))
                  (i32.const 0))
      (then (call $log (i32.const 48) (i32.const 14)))
      (else (call $log (i32.const 32) (i32.const 14)))))

  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $time_us i32)
    (local $start i32)
    (if (i32.ne (local.get $kind) (i32.const 2)) (then (return)))
    (if (i32.ne (local.get $source) (global.get $timer)) (then (return)))
    (global.set $count (i32.add (global.get $count) (local.get $value)))
    (global.set $led (i32.xor (global.get $led) (i32.const 1)))
    (drop (call $gpio_write (global.get $led_pin) (global.get $led)))
    (local.set $start (call $utoa (global.get $count)))
    (drop (call $publish (i32.const 16) (i32.const 15)
      (local.get $start) (i32.sub (i32.const 106) (local.get $start)))))
)
