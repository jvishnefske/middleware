/* nRF52833 (micro:bit v2): 512 KiB flash, 128 KiB RAM. Single image for now;
   the A/B slot layout comes with the OTA milestone. */
MEMORY
{
  FLASH : ORIGIN = 0x00000000, LENGTH = 512K
  RAM   : ORIGIN = 0x20000000, LENGTH = 128K
}
