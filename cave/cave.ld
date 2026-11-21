/* The code cave sits in the hole between the guest image's .bss and the HLE
   window. Absolute, never relocated: the host copies the bytes to exactly this
   address, which is also what every hook branch and literal assumes. */
ENTRY(cave_entry)
SECTIONS
{
  . = 0x011B0000;
  .text : { KEEP(*(.text.cave_entry)) *(.text .text.*) }
  .rodata : { *(.rodata .rodata.*) }
  . = ALIGN(16);
  .data : { *(.data .data.*) }
  . = ALIGN(16);
  .bss : { *(.bss .bss.*) *(COMMON) }
  /DISCARD/ : { *(.ARM.exidx*) *(.ARM.extab*) *(.comment) *(.note*) }
}
