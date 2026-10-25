;; ABI 2 fixture: button on GPIO 4 (pull-up, falling edge) toggles the LED
;; on GPIO 8 from on_irq and logs "press".
(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "gpio_input" (func $gpio_input (param i32 i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (import "mw" "gpio_irq" (func $gpio_irq (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "press")
  (global $led (mut i32) (i32.const 0))
  (func (export "init")
    (drop (call $gpio_input (i32.const 4) (i32.const 1)))
    (drop (call $gpio_output (i32.const 8)))
    (drop (call $gpio_irq (i32.const 4) (i32.const 2))))
  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $t i32)
    (if (i32.and (i32.eq (local.get $kind) (i32.const 1)) (i32.eq (local.get $source) (i32.const 4)))
      (then
        (global.set $led (i32.xor (global.get $led) (i32.const 1)))
        (drop (call $gpio_write (i32.const 8) (global.get $led)))
        (call $log (i32.const 16) (i32.const 5))))))
