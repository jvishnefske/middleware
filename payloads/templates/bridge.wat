;; Bridge template (hand-written WAT, ABI 6) for two-link boards such as
;; the Colorlight 5A-75B (two Ethernet ports, links 0 and 1).
;;
;; init installs two forwarding rules with route_bridge:
;;   link 0 -> link 1 for "demo/**" (demo, demo/a, demo/a/b, ...)
;;   link 1 -> link 0 for exactly "back/ack"
;; The device then forwards matching publications by itself; this payload
;; never sees them. It subscribes to "ctl/bridge": a message whose first
;; byte is the ASCII digit '0' stops the demo rule (route_stop), anything
;; else re-installs it. Every second, tick publishes the demo rule's forwarded count
;; (route_stat Frames, i32 little-endian, -7 = Busy once stopped) on
;; "bridge/stats".
;;
;; Memory uses 1-byte pages (custom-page-sizes): 4 KiB, well under the
;; 5A-75B's 16 KiB cap. The import and opcode subset (i32 only, no tables,
;; no floats) is what the gateware WASM core implements.
(module
  (import "mw" "log"          (func $log (param i32 i32)))
  (import "mw" "publish"      (func $publish (param i32 i32 i32 i32) (result i32)))
  (import "mw" "subscribe"    (func $subscribe (param i32 i32) (result i32)))
  (import "mw" "set_tick"     (func $set_tick (param i32) (result i32)))
  (import "mw" "route_stop"   (func $route_stop (param i32) (result i32)))
  (import "mw" "route_stat"   (func $route_stat (param i32 i32) (result i32)))
  (import "mw" "route_bridge" (func $route_bridge (param i32 i32 i32 i32) (result i32)))

  (memory (export "memory") 4096 4096 (pagesize 1))

  (data (i32.const 16)  "demo/**")            ;; 7 bytes
  (data (i32.const 32)  "back/ack")           ;; 8 bytes
  (data (i32.const 48)  "ctl/bridge")         ;; 10 bytes
  (data (i32.const 64)  "bridge/stats")       ;; 12 bytes
  (data (i32.const 80)  "bridge: up")         ;; 10 bytes
  (data (i32.const 96)  "bridge: refused")    ;; 15 bytes
  (data (i32.const 112) "bridge: stopped")    ;; 15 bytes

  ;; 4-byte scratch for the published count.
  (global $out i32 (i32.const 128))
  ;; Receive buffer for ctl/bridge.
  (global $rx i32 (i32.const 256))
  (global $rx_len i32 (i32.const 64))

  (global $demo (mut i32) (i32.const -1))
  (global $back (mut i32) (i32.const -1))
  (global $ctl (mut i32) (i32.const -1))

  (func $install_demo
    (global.set $demo
      (call $route_bridge (i32.const 0) (i32.const 16) (i32.const 7) (i32.const 1))))

  (func (export "init")
    (call $install_demo)
    (global.set $back
      (call $route_bridge (i32.const 1) (i32.const 32) (i32.const 8) (i32.const 0)))
    (global.set $ctl (call $subscribe (i32.const 48) (i32.const 10)))
    (drop (call $set_tick (i32.const 1000)))
    (if (i32.or (i32.lt_s (global.get $demo) (i32.const 0))
                (i32.lt_s (global.get $back) (i32.const 0)))
      (then (call $log (i32.const 96) (i32.const 15)))
      (else (call $log (i32.const 80) (i32.const 10)))))

  (func (export "tick") (param $now i32)
    (i32.store (global.get $out)
      (call $route_stat (global.get $demo) (i32.const 0)))
    (drop (call $publish (i32.const 64) (i32.const 12) (global.get $out) (i32.const 4))))

  (func (export "mw_alloc") (param $len i32) (result i32)
    (if (result i32) (i32.gt_u (local.get $len) (global.get $rx_len))
      (then (i32.const 0))
      (else (global.get $rx))))

  (func (export "on_message") (param $slot i32) (param $ptr i32) (param $len i32)
    (if (i32.ne (local.get $slot) (global.get $ctl)) (then (return)))
    (if (i32.eqz (local.get $len)) (then (return)))
    (if (i32.eq (i32.load8_u (local.get $ptr)) (i32.const 48))
      (then
        (drop (call $route_stop (global.get $demo)))
        (call $log (i32.const 112) (i32.const 15)))
      (else (call $install_demo))))
)
