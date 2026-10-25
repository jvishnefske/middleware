;; ABI 2 fixture: opens UART 1 (TX 4, RX 5, 115200) and echoes received
;; bytes; logs "sent" on each transmit DmaDone.
(module
  (import "mw" "log" (func $log (param i32 i32)))
  (import "mw" "uart_open" (func $uart_open (param i32 i32 i32 i32) (result i32)))
  (import "mw" "uart_read" (func $uart_read (param i32 i32 i32) (result i32)))
  (import "mw" "uart_write" (func $uart_write (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "sent")
  (func (export "init")
    (drop (call $uart_open (i32.const 1) (i32.const 4) (i32.const 5) (i32.const 115200))))
  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $t i32)
    (local $n i32)
    (if (i32.eq (local.get $kind) (i32.const 4))
      (then
        (local.set $n (call $uart_read (i32.const 1) (i32.const 256) (i32.const 64)))
        (if (i32.gt_s (local.get $n) (i32.const 0))
          (then (drop (call $uart_write (i32.const 1) (i32.const 256) (local.get $n)))))))
    (if (i32.and (i32.eq (local.get $kind) (i32.const 3)) (i32.eq (local.get $source) (i32.const 1)))
      (then (call $log (i32.const 16) (i32.const 4))))))
