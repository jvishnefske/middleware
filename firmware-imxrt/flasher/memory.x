/* imxrt-flasher: loaded by tools/ramload.py into the FlexRAM banking the
 * bootloader and the firmware use (8 OCRAM, 4 DTCM, 4 ITCM banks: GPR17
 * 0xFFAA5555, build.rs's FLEXRAM), so loading it changes no banks under a
 * running firmware (re-banking DTCM under the firmware reset the board,
 * 2026-11-13). FLASH: code and read-only data in DTCM, 64 KiB. RAM: the
 * 128 KiB buffer, the mailbox, .data, .bss and the stack in OCRAM (256 KiB;
 * the boot ROM, which uses it, never runs again before the next reset). */
MEMORY
{
  FLASH : ORIGIN = 0x20000000, LENGTH = 64K
  RAM   : ORIGIN = 0x20200000, LENGTH = 256K
}
__imxrt_family = 1050;
__flexram_config = 0xffaa5555;
/* imxrt-rt's __pre_init copies a section only when its load and run
 * addresses differ: make each pair equal. Not __srodata / __erodata, which
 * cortex-m-rt defines. */
PROVIDE(__sitext = __stext);
PROVIDE(__svector_table = 0);
PROVIDE(__sivector_table = 0);
PROVIDE(__evector_table = 0);
PROVIDE(__sirodata = __srodata);
PROVIDE(__eheap = 0);
