;; mw-pi-node test payload (ABI 5, hand-written WAT).
;;
;; init:       subscribes to "app/pi/in", serves "app/pi/echo" and
;;             "app/pi/trap", ticks every 20 ms, starts timer 0 (30 ms,
;;             periodic), tries GPIO 4 and logs "pi: gpio unsupported"
;;             when the node refuses it, then logs "pi: started".
;; tick:       publishes the tick count (u32 LE) on "app/pi/count".
;; on_message: republishes the message on "app/pi/out".
;; on_irq:     publishes the event's source and value on "app/pi/irq".
;; on_request: "app/pi/echo" answers node_id (8 bytes) then the body;
;;             "app/pi/trap" traps.
(module
  (import "mw" "log"         (func $log (param i32 i32)))
  (import "mw" "publish"     (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe"   (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "set_tick"    (func $set_tick (param i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "timer_start" (func $timer_start (param i32 i32 i32) (result i32)))
  (import "mw" "serve"       (func $serve (param i32 i32) (result i32)))
  (import "mw" "node_id"     (func $node_id (param i32) (result i32)))

  (memory (export "memory") 1 1)

  (data (i32.const 16)  "app/pi/in")            ;; 9
  (data (i32.const 32)  "app/pi/out")           ;; 10
  (data (i32.const 48)  "app/pi/count")         ;; 12
  (data (i32.const 64)  "app/pi/echo")          ;; 11
  (data (i32.const 80)  "app/pi/trap")          ;; 11
  (data (i32.const 96)  "app/pi/irq")           ;; 10
  (data (i32.const 112) "pi: started")          ;; 11
  (data (i32.const 128) "pi: gpio unsupported") ;; 20
  ;; 160..164 the tick count, 168..176 the irq report.

  (global $echo_slot (mut i32) (i32.const -1))
  (global $ticks (mut i32) (i32.const 0))

  (func (export "init")
    (drop (call $subscribe (i32.const 16) (i32.const 9)))
    (global.set $echo_slot (call $serve (i32.const 64) (i32.const 11)))
    (drop (call $serve (i32.const 80) (i32.const 11)))
    (drop (call $set_tick (i32.const 20)))
    (drop (call $timer_start (i32.const 0) (i32.const 30000) (i32.const 1)))
    (if (i32.eq (call $gpio_output (i32.const 4)) (i32.const -4))
      (then (call $log (i32.const 128) (i32.const 20))))
    (call $log (i32.const 112) (i32.const 11)))

  (func (export "tick") (param $now i32)
    (global.set $ticks (i32.add (global.get $ticks) (i32.const 1)))
    (i32.store (i32.const 160) (global.get $ticks))
    (drop (call $publish (i32.const 48) (i32.const 12) (i32.const 160) (i32.const 4))))

  (func (export "mw_alloc") (param $len i32) (result i32)
    (if (result i32) (i32.gt_u (local.get $len) (i32.const 2048))
      (then (i32.const 0))
      (else (i32.const 1024))))

  (func (export "on_message") (param $slot i32) (param $ptr i32) (param $len i32)
    (drop (call $publish (i32.const 32) (i32.const 10) (local.get $ptr) (local.get $len))))

  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $time i32)
    (i32.store (i32.const 168) (local.get $source))
    (i32.store (i32.const 172) (local.get $value))
    (drop (call $publish (i32.const 96) (i32.const 10) (i32.const 168) (i32.const 8))))

  (func (export "on_request") (param $slot i32) (param $req i32) (param $len i32)
        (param $meta i32) (param $reply i32) (param $cap i32) (result i32)
    (if (i32.ne (local.get $slot) (global.get $echo_slot))
      (then unreachable))
    (if (i32.lt_u (local.get $cap) (i32.add (local.get $len) (i32.const 8)))
      (then (return (i32.const -3))))
    (drop (call $node_id (local.get $reply)))
    (memory.copy (i32.add (local.get $reply) (i32.const 8)) (local.get $req) (local.get $len))
    (i32.add (local.get $len) (i32.const 8)))
)
