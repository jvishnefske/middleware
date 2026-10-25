;; ABI 2 fixture: a 1 ms periodic timer; on_irq adds each Timer event's
;; period count and publishes the running total (i32 LE) on app/ticks.
(module
  (import "mw" "timer_start" (func $timer_start (param i32 i32 i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "app/ticks")
  (global $count (mut i32) (i32.const 0))
  (func (export "init")
    (drop (call $timer_start (i32.const 0) (i32.const 1000) (i32.const 1))))
  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $t i32)
    (if (i32.eq (local.get $kind) (i32.const 2))
      (then
        (global.set $count (i32.add (global.get $count) (local.get $value)))
        (i32.store (i32.const 0) (global.get $count))
        (drop (call $publish (i32.const 16) (i32.const 9) (i32.const 0) (i32.const 4)))))))
