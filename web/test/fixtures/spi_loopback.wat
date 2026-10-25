;; ABI 2 fixture: init opens SPI bus 0 and sends de ad be ef; on DmaDone
;; for Spi0 it reads the received bytes with dma_read and publishes them on
;; app/spi.
(module
  (import "mw" "spi_open" (func $spi_open (param i32 i32 i32 i32 i32 i32 i32) (result i32)))
  (import "mw" "spi_transfer" (func $spi_transfer (param i32 i32 i32 i32) (result i32)))
  (import "mw" "dma_read" (func $dma_read (param i32 i32 i32) (result i32)))
  (import "mw" "publish" (func $publish (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 16) "app/spi")
  (data (i32.const 64) "\de\ad\be\ef")
  (func (export "init")
    (drop (call $spi_open (i32.const 0) (i32.const 6) (i32.const 7) (i32.const 2)
                          (i32.const 10) (i32.const 1000) (i32.const 0)))
    (drop (call $spi_transfer (i32.const 0) (i32.const 64) (i32.const 4) (i32.const 4))))
  (func (export "on_irq") (param $kind i32) (param $source i32) (param $value i32) (param $t i32)
    (local $n i32)
    (if (i32.and (i32.eq (local.get $kind) (i32.const 3)) (i32.eqz (local.get $source)))
      (then
        (local.set $n (call $dma_read (i32.const 0) (i32.const 128) (i32.const 16)))
        (if (i32.gt_s (local.get $n) (i32.const 0))
          (then (drop (call $publish (i32.const 16) (i32.const 7) (i32.const 128) (local.get $n)))))))))
