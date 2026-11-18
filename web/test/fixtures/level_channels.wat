;; ABI 7 channels (BD5, BD6): opens `audio` (an input) and `level` (an
;; output) by name and republishes every `audio` message on `level`. The
;; `mw.channels` section is appended by the test (protocol-wasm
;; `withChannels`), since the WAT has no section syntax the browser's wabt
;; build accepts.
(module
  (import "mw" "channel_open" (func $open (param i32 i32) (result i32)))
  (import "mw" "channel_publish" (func $pub (param i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 32) "audio")
  (data (i32.const 48) "level")
  (func (export "init")
    (i32.store (i32.const 100) (call $open (i32.const 32) (i32.const 5)))
    (i32.store (i32.const 104) (call $open (i32.const 48) (i32.const 5))))
  (func (export "mw_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_message") (param $slot i32) (param $p i32) (param $n i32)
    (drop (call $pub (i32.load (i32.const 104)) (local.get $p) (local.get $n)))))
