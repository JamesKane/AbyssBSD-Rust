/* SPDX-License-Identifier: BSD-2-Clause */
/*
 * The kernel image: a static PIE linked at KERNEL_LINK_BASE
 * (bootproto::layout), slid by the loader. Three segments, R+X, R, and
 * R+W, so the kernel can map them W^X. Everything the loader relocates
 * lives in the R segment: it applies relocations before the permissions
 * take effect.
 *
 * Each segment starts on a 2 MiB boundary and the kernel maps it to the
 * next one, so the image is mapped with 2 MiB leaves (the link base and
 * the loader's slide and load address are 2 MiB-aligned too), as
 * FreeBSD maps its kernel (arm64 locore.S, amd64 pmap_bootstrap). A
 * core with a small first-level TLB then holds the image in a few
 * entries. The padding costs memory, not file size, and the loader
 * zeroes it.
 */
ENTRY(_start)

PHDRS
{
    text    PT_LOAD FLAGS(5);
    rodata  PT_LOAD FLAGS(4);
    data    PT_LOAD FLAGS(6);
    dynamic PT_DYNAMIC FLAGS(4);
}

SECTIONS
{
    . = 0xffffffff80000000;
    __kernel_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text.trap)
        *(.text .text.*)
    } :text

    . = ALIGN(0x200000);
    __text_end = .;

    .rodata : { *(.rodata .rodata.*) } :rodata
    .data.rel.ro : { *(.data.rel.ro .data.rel.ro.*) } :rodata
    .got : { *(.got .got.plt) } :rodata
    abyss_extable : {
        __start_abyss_extable = .;
        KEEP(*(abyss_extable))
        __stop_abyss_extable = .;
    } :rodata
    ktest : {
        __start_ktest = .;
        KEEP(*(ktest))
        __stop_ktest = .;
    } :rodata
    .dynamic : { *(.dynamic) } :rodata :dynamic
    .dynsym : { *(.dynsym) } :rodata
    .dynstr : { *(.dynstr) } :rodata
    .hash : { *(.hash) } :rodata
    .gnu.hash : { *(.gnu.hash) } :rodata
    .rela.dyn : { *(.rela.dyn .rela.*) } :rodata

    . = ALIGN(0x200000);
    __rodata_end = .;

    .data : { *(.data .data.*) } :data
    .bss (NOLOAD) : {
        *(.bss .bss.*)
        *(COMMON)
        /*
         * The boot stack, with an unmapped guard page below it. They
         * follow the 2 MiB leaves of the data, in 4 KiB pages of their
         * own: a hole inside a leaf would split it.
         */
        . = ALIGN(0x200000);
        __boot_stack_guard = .;
        . += 4096;
        . += 65536;
        __boot_stack_top = .;
    } :data

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr .eh_frame.*)
        *(.note .note.*)
        *(.comment)
        *(.interp)
    }
}
