;; ABI 2 fixture: arms GPIO 4 on both edges; each on_irq asks irq_overflow
;; and publishes a non-zero drop count (i32 LE) on app/overflow.
(module
  (import "mw" "gpio_input" (func $gpio_input (param i32 i32) (result i32)))
  (import "mw" "gpio_irq" (func $gpio_irq (param i32 i32) (result i32)))
  (import "mw" "irq_overflow" (func $irq_overflow (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "app/overflow")
  (func (export "init")
    (drop (call $gpio_input (i32.const 4) (i32.const 1)))
    (drop (call $gpio_irq (i32.const 4) (i32.const 3))))
  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $t i32)
    (local $o i32)
    (local.set $o (call $irq_overflow))
    (if (local.get $o)
      (then
        (i32.store (i32.const 0) (local.get $o))
        (drop (call $publish (i32.const 16) (i32.const 12) (i32.const 0) (i32.const 4)))))))
