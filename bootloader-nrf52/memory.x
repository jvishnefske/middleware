/* nRF52833 (micro:bit v2): 512 KiB flash, 128 KiB RAM. This bootloader owns
   the first 16 KiB. The application is linked at 0x4000 by its own crate and
   is not linked here. The FLASH region is the size budget: if .text + .rodata
   + .data (LMA) exceed 16 KiB the link fails with "region FLASH overflowed". */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 16K
  RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}
