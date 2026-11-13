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
    . = ALIGN(8);
    __esdram_bss = .;
  } > FLASH
}
INSERT BEFORE .bss;
