/* imxrt-base (the firmware): runs from SDRAM, where the bootloader copied
 * and checked it.
 *
 * FLASH is the SDRAM image region (`imxrt_base::layout::SDRAM_IMAGE`,
 * 0x8000_0000, 4 MiB = the slot size): vector table, code, read-only data
 * and the .data image. RAM is DTCM (128 KiB with the FlexRAM banks below):
 * the stack, .data and .bss.
 */
MEMORY
{
  FLASH : ORIGIN = 0x80000000, LENGTH = 4M
  RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}

/* imxrt-rt's __pre_init (linked through embassy-nxp) sets the FlexRAM banks
 * from __flexram_config; build.rs writes the same value the bootloader
 * uses, so the banking does not change under the firmware. */
__imxrt_family = 1050;
__flexram_config = @FLEXRAM_CONFIG@;

/* __pre_init copies a section only when its load and run addresses differ;
 * the firmware is already where it runs, so make each pair equal. Do not
 * PROVIDE __srodata / __erodata: cortex-m-rt defines them, and a PROVIDE'd
 * value here would win in the __sirodata expression. */
PROVIDE(__sitext = __stext);
PROVIDE(__svector_table = 0);
PROVIDE(__sivector_table = 0);
PROVIDE(__evector_table = 0);
PROVIDE(__sirodata = __srodata);
PROVIDE(__eheap = 0);

/* Statics that need no single-cycle access and no DMA live in SDRAM, so
 * DTCM keeps the stack and the DMA memory (LPUART1's ring, USB's queue
 * heads, descriptors and buffers, ENET's rings and frame buffers):
 *
 * - .sdram_data: initialised statics named `*_SDRAM` (the bus and its
 *   publication queues, `app::BUS_SDRAM`), placed in the image itself
 *   after .rodata: the bootloader copied the whole image into SDRAM, so
 *   their initial values are already where they run (cortex-m-rt's .data
 *   copy is for DTCM only).
 * - .sdram_bss (below): zero-initialised statics: the embassy task pools
 *   (`*4POOL*`, the mangled `POOL` of every `#[embassy_executor::task]`),
 *   statics named `*_SDRAM`, and the payload heap's arena.
 */
SECTIONS
{
  .sdram_data : ALIGN(4)
  {
    *(.data.*_SDRAM*)
    . = ALIGN(4);
  } > FLASH
}
INSERT BEFORE .data;

/* The payload heap's arena (`app::heap::HEAP_ARENA`, feature `wasm`) lives
 * in SDRAM, in the image region right after the loaded image: a NOLOAD
 * section, so it is not part of the slot image, and inserted before
 * cortex-m-rt's .bss so this rule claims the arena first (the same way
 * boot-image.x places the bootloader's APP_IMAGE). cortex-m-rt zeroes only
 * .bss in RAM (DTCM); the bootloader zeroes the rest of the image region
 * after the image before it jumps (`imxrt-boot`), so this section starts
 * zeroed like .bss (the arena's cell flag must read "not taken"). */
SECTIONS
{
  .sdram_bss (NOLOAD) : ALIGN(8)
  {
    __ssdram_bss = .;
    *(.bss.*HEAP_ARENA* .uninit.*HEAP_ARENA*)
    *(.bss.*4POOL* .bss.*_SDRAM*)
    . = ALIGN(8);
    __esdram_bss = .;
  } > FLASH
}
INSERT BEFORE .bss;
