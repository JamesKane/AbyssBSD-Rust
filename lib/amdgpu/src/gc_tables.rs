// SPDX-License-Identifier: BSD-2-Clause
//! Tables of values from specification 06 for the graphics block of
//! Raphael (GC 10.3.6), generated from the specification's own tables:
//! the clear-state buffer's non-zero context register values (section 6.2)
//! and the golden register settings (section 3.1).

/// The clear-state buffer: its context registers' reset values that are not
/// zero, (packet offset in the context register space, value), in order of
/// offset. 85 of the 921 values (06 section 6.2).
pub(crate) const CSB_NONZERO: [(u16, u32); 85] = [
    (0x00d, 0x40004000), // PA_SC_SCREEN_SCISSOR_BR
    (0x01f, 0x00150055), // DB_RMI_L2_CACHE_CONTROL
    (0x081, 0x80000000), // PA_SC_WINDOW_SCISSOR_TL
    (0x082, 0x40004000), // PA_SC_WINDOW_SCISSOR_BR
    (0x083, 0x0000ffff), // PA_SC_CLIPRECT_RULE
    (0x085, 0x40004000), // PA_SC_CLIPRECT_0_BR
    (0x087, 0x40004000), // PA_SC_CLIPRECT_1_BR
    (0x089, 0x40004000), // PA_SC_CLIPRECT_2_BR
    (0x08b, 0x40004000), // PA_SC_CLIPRECT_3_BR
    (0x08c, 0xaa99aaaa), // PA_SC_EDGERULE
    (0x08e, 0xffffffff), // CB_TARGET_MASK
    (0x08f, 0xffffffff), // CB_SHADER_MASK
    (0x090, 0x80000000), // PA_SC_GENERIC_SCISSOR_TL
    (0x091, 0x40004000), // PA_SC_GENERIC_SCISSOR_BR
    (0x094, 0x80000000), // PA_SC_VPORT_SCISSOR_0_TL
    (0x095, 0x40004000), // PA_SC_VPORT_SCISSOR_0_BR
    (0x096, 0x80000000), // PA_SC_VPORT_SCISSOR_1_TL
    (0x097, 0x40004000), // PA_SC_VPORT_SCISSOR_1_BR
    (0x098, 0x80000000), // PA_SC_VPORT_SCISSOR_2_TL
    (0x099, 0x40004000), // PA_SC_VPORT_SCISSOR_2_BR
    (0x09a, 0x80000000), // PA_SC_VPORT_SCISSOR_3_TL
    (0x09b, 0x40004000), // PA_SC_VPORT_SCISSOR_3_BR
    (0x09c, 0x80000000), // PA_SC_VPORT_SCISSOR_4_TL
    (0x09d, 0x40004000), // PA_SC_VPORT_SCISSOR_4_BR
    (0x09e, 0x80000000), // PA_SC_VPORT_SCISSOR_5_TL
    (0x09f, 0x40004000), // PA_SC_VPORT_SCISSOR_5_BR
    (0x0a0, 0x80000000), // PA_SC_VPORT_SCISSOR_6_TL
    (0x0a1, 0x40004000), // PA_SC_VPORT_SCISSOR_6_BR
    (0x0a2, 0x80000000), // PA_SC_VPORT_SCISSOR_7_TL
    (0x0a3, 0x40004000), // PA_SC_VPORT_SCISSOR_7_BR
    (0x0a4, 0x80000000), // PA_SC_VPORT_SCISSOR_8_TL
    (0x0a5, 0x40004000), // PA_SC_VPORT_SCISSOR_8_BR
    (0x0a6, 0x80000000), // PA_SC_VPORT_SCISSOR_9_TL
    (0x0a7, 0x40004000), // PA_SC_VPORT_SCISSOR_9_BR
    (0x0a8, 0x80000000), // PA_SC_VPORT_SCISSOR_10_TL
    (0x0a9, 0x40004000), // PA_SC_VPORT_SCISSOR_10_BR
    (0x0aa, 0x80000000), // PA_SC_VPORT_SCISSOR_11_TL
    (0x0ab, 0x40004000), // PA_SC_VPORT_SCISSOR_11_BR
    (0x0ac, 0x80000000), // PA_SC_VPORT_SCISSOR_12_TL
    (0x0ad, 0x40004000), // PA_SC_VPORT_SCISSOR_12_BR
    (0x0ae, 0x80000000), // PA_SC_VPORT_SCISSOR_13_TL
    (0x0af, 0x40004000), // PA_SC_VPORT_SCISSOR_13_BR
    (0x0b0, 0x80000000), // PA_SC_VPORT_SCISSOR_14_TL
    (0x0b1, 0x40004000), // PA_SC_VPORT_SCISSOR_14_BR
    (0x0b2, 0x80000000), // PA_SC_VPORT_SCISSOR_15_TL
    (0x0b3, 0x40004000), // PA_SC_VPORT_SCISSOR_15_BR
    (0x0b5, 0x3f800000), // PA_SC_VPORT_ZMAX_0
    (0x0b7, 0x3f800000), // PA_SC_VPORT_ZMAX_1
    (0x0b9, 0x3f800000), // PA_SC_VPORT_ZMAX_2
    (0x0bb, 0x3f800000), // PA_SC_VPORT_ZMAX_3
    (0x0bd, 0x3f800000), // PA_SC_VPORT_ZMAX_4
    (0x0bf, 0x3f800000), // PA_SC_VPORT_ZMAX_5
    (0x0c1, 0x3f800000), // PA_SC_VPORT_ZMAX_6
    (0x0c3, 0x3f800000), // PA_SC_VPORT_ZMAX_7
    (0x0c5, 0x3f800000), // PA_SC_VPORT_ZMAX_8
    (0x0c7, 0x3f800000), // PA_SC_VPORT_ZMAX_9
    (0x0c9, 0x3f800000), // PA_SC_VPORT_ZMAX_10
    (0x0cb, 0x3f800000), // PA_SC_VPORT_ZMAX_11
    (0x0cd, 0x3f800000), // PA_SC_VPORT_ZMAX_12
    (0x0cf, 0x3f800000), // PA_SC_VPORT_ZMAX_13
    (0x0d1, 0x3f800000), // PA_SC_VPORT_ZMAX_14
    (0x0d3, 0x3f800000), // PA_SC_VPORT_ZMAX_15
    (0x100, 0xffffffff), // VGT_MAX_VTX_INDX
    (0x104, 0x00550055), // CB_RMI_GL2_CACHE_CONTROL
    (0x10c, 0x01000000), // DB_STENCILREFMASK
    (0x10d, 0x01000000), // DB_STENCILREFMASK_BF
    (0x1b6, 0x00000002), // SPI_PS_IN_CONTROL
    (0x204, 0x00090000), // PA_CL_CLIP_CNTL
    (0x205, 0x00000004), // PA_SU_SC_MODE_CNTL
    (0x295, 0x00000100), // VGT_GS_PER_ES
    (0x296, 0x00000080), // VGT_ES_PER_GS
    (0x297, 0x00000002), // VGT_GS_PER_VS
    (0x2aa, 0x000000ff), // IA_MULTI_VGT_PARAM
    (0x2f7, 0x00001000), // PA_SC_LINE_CNTL
    (0x2f9, 0x00000005), // PA_SU_VTX_CNTL
    (0x2fa, 0x3f800000), // PA_CL_GB_VERT_CLIP_ADJ
    (0x2fb, 0x3f800000), // PA_CL_GB_VERT_DISC_ADJ
    (0x2fc, 0x3f800000), // PA_CL_GB_HORZ_CLIP_ADJ
    (0x2fd, 0x3f800000), // PA_CL_GB_HORZ_DISC_ADJ
    (0x30e, 0xffffffff), // PA_SC_AA_MASK_X0Y0_X1Y0
    (0x30f, 0xffffffff), // PA_SC_AA_MASK_X0Y1_X1Y1
    (0x311, 0x00000003), // PA_SC_BINNER_CNTL_0
    (0x313, 0x00100000), // PA_SC_CONSERVATIVE_RASTERIZATION_CNTL
    (0x316, 0x0000001e), // VGT_VERTEX_REUSE_BLOCK_CNTL
    (0x317, 0x00000020), // VGT_OUT_DEALLOC_CNTL
];

/// The golden settings for GC 10.3.6 (06 section 3.1): (name, register byte
/// offset on Raphael, mask, value). A mask of all ones writes the value; any
/// other reads, clears the mask bits and ORs in the value.
pub(crate) const GOLDEN_10_3_6: [(&str, usize, u32, u32); 22] = [
    ("CGTT_SPI_CS_CLK_CTRL", 0x3c1f0, 0xff7f0fff, 0x78000100),
    ("CH_PIPE_STEER", 0x33640, 0x000000ff, 0x00000044),
    ("CPF_GCR_CNTL", 0x0c6cc, 0x0007ffff, 0x0000c200),
    ("DB_DEBUG3", 0x09838, 0xffffffff, 0x00000280),
    ("DB_DEBUG4", 0x0983c, 0xffffffff, 0x00800000),
    ("GB_ADDR_CONFIG", 0x098f8, 0x0c1807ff, 0x00000042),
    ("GCR_GENERAL_CNTL", 0x09f80, 0x1ff1ffff, 0x00000500),
    ("GL1_PIPE_STEER", 0x33440, 0x000000ff, 0x00000044),
    ("GL2_PIPE_STEER_0", 0x33894, 0x77777777, 0x32103210),
    ("GL2_PIPE_STEER_1", 0x33898, 0x77777777, 0x32103210),
    ("GL2A_ADDR_MATCH_MASK", 0x33884, 0xffffffff, 0xfffffff3),
    ("GL2C_ADDR_MATCH_MASK", 0x3380c, 0xffffffff, 0xfffffff3),
    ("GL2C_CM_CTRL1", 0x33820, 0xff8fff0f, 0x580f1008),
    ("GL2C_CTRL3", 0x33830, 0xf7ffffff, 0x00f80988),
    ("LDS_CONFIG", 0x08c08, 0x000001ff, 0x00000020),
    ("PA_CL_ENHANCE", 0x08a14, 0xf17fffff, 0x01200007),
    (
        "PA_SC_BINNER_TIMEOUT_COUNTER",
        0x08b40,
        0xffffffff,
        0x00000800,
    ),
    ("PA_SC_ENHANCE_2", 0x08b70, 0xffffffbf, 0x00000820),
    ("SQG_CONFIG", 0x08c68, 0x000017ff, 0x00001000),
    ("SX_DEBUG_1", 0x09060, 0xffffff7f, 0x00010020),
    ("TA_CNTL_AUX", 0x09508, 0xfff7ffff, 0x01030000),
    ("UTCL1_CTRL", 0x09fa0, 0xffffffff, 0x00100000),
];
