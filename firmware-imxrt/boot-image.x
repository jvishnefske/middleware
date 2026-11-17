/* imxrt-boot: the SDRAM image region the bootloader copies a slot into.
 *
 * `APP_IMAGE` (a static of atomics in src/bin/imxrt-boot.rs) is taken out of
 * .bss and placed at the firmware's link address, 0x8000_0000, the start
 * of SDRAM (initialised by the boot header's DCD before the bootloader
 * runs). NOLOAD: the startup does not zero 4 MiB of SDRAM; the bootloader
 * writes every byte it later reads. The size is the slot size and
 * `imxrt_base::layout::SDRAM_IMAGE`.
 *
 * build.rs splices the three parts below into imxrt-rt's generated
 * imxrt-boot.x: the region before its MEMORY use, the output section just
 * before its `.bss` (lld matches input sections in script order, and an
 * INSERT BEFORE .bss is matched after .bss's `*(.bss .bss.*)`), and the
 * assertions at the end.
 */
/* @MEMORY */
MEMORY
{
  SDRAM_IMAGE : ORIGIN = 0x80000000, LENGTH = 4M
}
/* @SECTION */
  .app_image (NOLOAD) : ALIGN(1024)
  {
    __app_image_start = .;
    KEEP(*(.bss.*APP_IMAGE*));
    __app_image_end = .;
  } > SDRAM_IMAGE
/* @ASSERT */
ASSERT(__app_image_start == ORIGIN(SDRAM_IMAGE), "
ERROR(imxrt-boot): APP_IMAGE must sit at the firmware's link address (0x80000000).");
ASSERT(__app_image_end - __app_image_start == LENGTH(SDRAM_IMAGE), "
ERROR(imxrt-boot): APP_IMAGE must be exactly the SDRAM image region (4 MiB).");
