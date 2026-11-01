;; ABI 5 fixture: GPIO 5 is an output, then becomes the data pin of a
;; 3-LED strip; init writes red, green, blue (G, R, B wire bytes) and
;; publishes the codes of gpio_write(5) on "app/gpio" and of a 4-LED
;; frame on "app/long" (both InvalidArgument). Each message on "app/in"
;; writes its first 3 bytes to the first LED; a message on "app/off"
;; closes the strip. Strips raise no events, so no on_irq is exported.
(module
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "gpio_output" (func $gpio_output (param i32) (result i32)))
  (import "mw" "gpio_write" (func $gpio_write (param i32 i32) (result i32)))
  (import "mw" "ledstrip_open" (func $open (param i32 i32) (result i32)))
  (import "mw" "ledstrip_write" (func $write (param i32 i32 i32) (result i32)))
  (import "mw" "ledstrip_close" (func $close (param i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "app/in")
  (data (i32.const 8) "app/off")
  (data (i32.const 16) "app/gpio")
  (data (i32.const 32) "app/long")
  (data (i32.const 64) "\00\ff\00\ff\00\00\00\00\ff")
  (global $h (mut i32) (i32.const -1))
  (func (export "init")
    (drop (call $gpio_output (i32.const 5)))
    (global.set $h (call $open (i32.const 5) (i32.const 3)))
    (drop (call $write (global.get $h) (i32.const 64) (i32.const 9)))
    (i32.store (i32.const 128) (call $gpio_write (i32.const 5) (i32.const 1)))
    (drop (call $publish (i32.const 16) (i32.const 8) (i32.const 128) (i32.const 4)))
    (i32.store (i32.const 128) (call $write (global.get $h) (i32.const 64) (i32.const 12)))
    (drop (call $publish (i32.const 32) (i32.const 8) (i32.const 128) (i32.const 4)))
    (drop (call $subscribe (i32.const 0) (i32.const 6)))
    (drop (call $subscribe (i32.const 8) (i32.const 7))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param $slot i32) (param $ptr i32) (param $len i32)
    (if (i32.eqz (local.get $slot))
      (then (drop (call $write (global.get $h) (local.get $ptr) (i32.const 3))))
      (else (drop (call $close (global.get $h)))))))
