/* RP2350A (Raspberry Pi Pico 2, 4 MiB QSPI flash, 520 KiB SRAM).
 *
 * FLASH is firmware slot A of the flash map in src/layout.rs
 * (0x000000..0x180000) minus 8 KiB kept for the partition table the A/B
 * firmware OTA work will add at the start of flash (the RP2350 boot ROM
 * maps a partition at 0x10000000 through address translation, so an image
 * linked here runs from either slot). Slot B, the payload store and the
 * trust record lie above it and are never linked into. RAM is SRAM0-7 plus
 * SRAM8 and SRAM9, contiguous.
 */
MEMORY {
    FLASH : ORIGIN = 0x10000000, LENGTH = 0x17E000
    RAM   : ORIGIN = 0x20000000, LENGTH = 0x82000
}

/* The boot ROM looks for the IMAGE_DEF block (embassy-rp emits it in
 * .start_block) near the start of the image. */
SECTIONS {
    .start_block : ALIGN(4)
    {
        __start_block_addr = .;
        KEEP(*(.start_block));
        KEEP(*(.boot_info));
    } > FLASH
} INSERT AFTER .vector_table;

/* .text starts after the boot info. */
_stext = ADDR(.start_block) + SIZEOF(.start_block);

SECTIONS {
    .bi_entries : ALIGN(4)
    {
        __bi_entries_start = .;
        KEEP(*(.bi_entries));
        . = ALIGN(4);
        __bi_entries_end = .;
    } > FLASH
} INSERT AFTER .text;

SECTIONS {
    .end_block : ALIGN(4)
    {
        __end_block_addr = .;
        KEEP(*(.end_block));
    } > FLASH
} INSERT AFTER .uninit;

PROVIDE(start_to_end = __end_block_addr - __start_block_addr);
PROVIDE(end_to_start = __start_block_addr - __end_block_addr);
