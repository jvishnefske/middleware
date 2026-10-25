;; Echo template (hand-written WAT for the middleware payload ABI v1).
;;
;; Subscribes to "app/echo/in" and republishes each message on
;; "app/echo/out". Message delivery works in two steps: the host calls
;; mw_alloc(len) to get a buffer in this module's memory, copies the message
;; there, then calls on_message(slot, ptr, len). This module keeps one fixed
;; 512-byte (MAX_MESSAGE) receive buffer at offset 1024 and returns 0 when a
;; message would not fit.
(module
  (import "mw" "log"       (func $log (param i32 i32)))
  (import "mw" "publish"   (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe" (func $subscribe (param i32 i32) (result i32)))

  (memory (export "memory") 1 1)

  (data (i32.const 16) "app/echo/in")       ;; 11 bytes
  (data (i32.const 32) "app/echo/out")      ;; 12 bytes
  (data (i32.const 48) "echo: started")     ;; 13 bytes
  (data (i32.const 64) "echo: message")     ;; 13 bytes
  (data (i32.const 80) "echo: subscribe failed") ;; 22 bytes

  (global $rx i32 (i32.const 1024))
  (global $rx_len i32 (i32.const 512))
  ;; Slot returned by subscribe, or -1.
  (global $slot (mut i32) (i32.const -1))

  (func (export "init")
    (global.set $slot (call $subscribe (i32.const 16) (i32.const 11)))
    (if (i32.lt_s (global.get $slot) (i32.const 0))
      (then (call $log (i32.const 80) (i32.const 22)))
      (else (call $log (i32.const 48) (i32.const 13)))))

  (func (export "mw_alloc") (param $len i32) (result i32)
    (if (result i32) (i32.gt_u (local.get $len) (global.get $rx_len))
      (then (i32.const 0))
      (else (global.get $rx))))

  (func (export "on_message") (param $slot i32) (param $ptr i32) (param $len i32)
    (if (i32.ne (local.get $slot) (global.get $slot)) (then (return)))
    (call $log (i32.const 64) (i32.const 13))
    (drop (call $publish (i32.const 32) (i32.const 12) (local.get $ptr) (local.get $len))))
)
