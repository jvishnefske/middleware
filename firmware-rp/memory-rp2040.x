/* RP2040 (Raspberry Pi Pico, 2 MiB QSPI flash, 264 KiB SRAM).
 *
 * FLASH is the firmware region of the flash map in src/layout.rs
 * (0x000000..0x180000, 1.5 MiB) minus the 256-byte second-stage
 * bootloader that embassy-rp places in BOOT2 (link-rp.x). The payload
 * store and the trust record lie above it and are never linked into.
 * RAM is the striped 256 KiB (SRAM0-3) plus SRAM4 and SRAM5, contiguous.
 */
MEMORY {
    BOOT2 : ORIGIN = 0x10000000, LENGTH = 0x100
    FLASH : ORIGIN = 0x10000100, LENGTH = 0x17FF00
    RAM   : ORIGIN = 0x20000000, LENGTH = 0x42000
}

/* The stack's room (D51): between the end of the statics (cortex-m-rt's
 * __euninit, after .bss with the heap arena and .uninit) and the stack top
 * (_stack_start, the end of RAM) there must be the deepest poll's stack
 * and a margin, rp_base::chip::net_budget::{STACK, MARGIN} (a unit test
 * keeps the figure equal to their sum). Nothing guards the stack at run
 * time, so growing statics fail here instead of corrupting .bss. */
ASSERT(_stack_start - __euninit >= 52824, "
ERROR(rp-base): less than STACK + MARGIN (52824 bytes) of RAM is left for the stack; see rp_base::chip::net_budget");
