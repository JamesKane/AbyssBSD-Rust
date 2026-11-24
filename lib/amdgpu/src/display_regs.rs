// SPDX-License-Identifier: MIT
// Copyright Advanced Micro Devices, Inc.
// Copyright (c) 2026 The AbyssBSD Project
//! The display registers read once at boot, before any driver programs the
//! display (specification 08, section 2.6): byte offsets in the register window
//! for Raphael (DCN 3.1.5), one per instance.
//!
//! Transcribed from the specification's table by a script; the offsets were
//! computed there from the IP discovery bases of 01 section 9.1.

/// A register and where each of its instances is (byte offsets).
pub const REGISTERS: &[(&str, &[usize])] = &[
    ("OTG<i>_OTG_CONTROL", &[0x14004, 0x14204, 0x14404, 0x14604]),
    ("OTG<i>_OTG_H_TOTAL", &[0x13fa8, 0x141a8, 0x143a8, 0x145a8]),
    (
        "OTG<i>_OTG_H_BLANK_START_END",
        &[0x13fac, 0x141ac, 0x143ac, 0x145ac],
    ),
    ("OTG<i>_OTG_H_SYNC_A", &[0x13fb0, 0x141b0, 0x143b0, 0x145b0]),
    (
        "OTG<i>_OTG_H_SYNC_A_CNTL",
        &[0x13fb4, 0x141b4, 0x143b4, 0x145b4],
    ),
    ("OTG<i>_OTG_V_TOTAL", &[0x13fbc, 0x141bc, 0x143bc, 0x145bc]),
    (
        "OTG<i>_OTG_V_BLANK_START_END",
        &[0x13fd8, 0x141d8, 0x143d8, 0x145d8],
    ),
    ("OTG<i>_OTG_V_SYNC_A", &[0x13fdc, 0x141dc, 0x143dc, 0x145dc]),
    (
        "OTG<i>_OTG_V_SYNC_A_CNTL",
        &[0x13fe0, 0x141e0, 0x143e0, 0x145e0],
    ),
    (
        "OTG<i>_OTG_INTERLACE_CONTROL",
        &[0x14010, 0x14210, 0x14410, 0x14610],
    ),
    (
        "OTG<i>_OTG_STATUS_POSITION",
        &[0x14028, 0x14228, 0x14428, 0x14628],
    ),
    (
        "ODM<i>_OPTC_DATA_SOURCE_SELECT",
        &[0x13e2c, 0x13e6c, 0x13eac, 0x13eec],
    ),
    (
        "OTG<i>_PIXEL_RATE_CNTL",
        &[0x00500, 0x00510, 0x00520, 0x00530],
    ),
    ("DP_DTO<i>_PHASE", &[0x00504, 0x00514, 0x00524, 0x00534]),
    ("DP_DTO<i>_MODULO", &[0x00508, 0x00518, 0x00528, 0x00538]),
    ("HUBP<i>_DCHUBP_CNTL", &[0x0eacc, 0x0ee3c, 0x0f1ac, 0x0f51c]),
    (
        "HUBP<i>_HUBP_CLK_CNTL",
        &[0x0ead0, 0x0ee40, 0x0f1b0, 0x0f520],
    ),
    (
        "HUBP<i>_DCSURF_SURFACE_CONFIG",
        &[0x0ea94, 0x0ee04, 0x0f174, 0x0f4e4],
    ),
    (
        "HUBP<i>_DCSURF_TILING_CONFIG",
        &[0x0ea9c, 0x0ee0c, 0x0f17c, 0x0f4ec],
    ),
    (
        "HUBP<i>_DCSURF_ADDR_CONFIG",
        &[0x0ea98, 0x0ee08, 0x0f178, 0x0f4e8],
    ),
    (
        "HUBP<i>_DCSURF_PRI_VIEWPORT_START",
        &[0x0eaa4, 0x0ee14, 0x0f184, 0x0f4f4],
    ),
    (
        "HUBP<i>_DCSURF_PRI_VIEWPORT_DIMENSION",
        &[0x0eaa8, 0x0ee18, 0x0f188, 0x0f4f8],
    ),
    (
        "HUBPREQ<i>_DCSURF_SURFACE_PITCH",
        &[0x0eb1c, 0x0ee8c, 0x0f1fc, 0x0f56c],
    ),
    (
        "HUBPREQ<i>_DCSURF_PRIMARY_SURFACE_ADDRESS",
        &[0x0eb28, 0x0ee98, 0x0f208, 0x0f578],
    ),
    (
        "HUBPREQ<i>_DCSURF_PRIMARY_SURFACE_ADDRESS_HIGH",
        &[0x0eb2c, 0x0ee9c, 0x0f20c, 0x0f57c],
    ),
    (
        "HUBPREQ<i>_DCSURF_SURFACE_CONTROL",
        &[0x0eb68, 0x0eed8, 0x0f248, 0x0f5b8],
    ),
    (
        "HUBPREQ<i>_DCSURF_FLIP_CONTROL",
        &[0x0eb6c, 0x0eedc, 0x0f24c, 0x0f5bc],
    ),
    (
        "HUBPREQ<i>_VMID_SETTINGS_0",
        &[0x0eb24, 0x0ee94, 0x0f204, 0x0f574],
    ),
    (
        "HUBPRET<i>_HUBPRET_CONTROL",
        &[0x0ecb0, 0x0f020, 0x0f390, 0x0f700],
    ),
    (
        "MPCC<i>_MPCC_TOP_SEL",
        &[0x24000, 0x24080, 0x24100, 0x24180],
    ),
    (
        "MPCC<i>_MPCC_BOT_SEL",
        &[0x24004, 0x24084, 0x24104, 0x24184],
    ),
    ("MPCC<i>_MPCC_OPP_ID", &[0x24008, 0x24088, 0x24108, 0x24188]),
    (
        "MPCC<i>_MPCC_CONTROL",
        &[0x2400c, 0x2408c, 0x2410c, 0x2418c],
    ),
    ("MPC_OUT<i>_MUX", &[0x25600, 0x25610, 0x25620, 0x25630]),
    (
        "OPP_PIPE<i>_OPP_PIPE_CONTROL",
        &[0x13530, 0x13698, 0x13800, 0x13968],
    ),
    ("FMT<i>_FMT_CONTROL", &[0x13400, 0x13568, 0x136d0, 0x13838]),
    (
        "FMT<i>_FMT_BIT_DEPTH_CONTROL",
        &[0x13404, 0x1356c, 0x136d4, 0x1383c],
    ),
    (
        "CNVC_CFG<i>_FORMAT_CONTROL",
        &[0x10640, 0x10bec, 0x11198, 0x11744],
    ),
    (
        "CNVC_CFG<i>_CNVC_SURFACE_PIXEL_FORMAT",
        &[0x1063c, 0x10be8, 0x11194, 0x11740],
    ),
    (
        "DPP_TOP<i>_DPP_CONTROL",
        &[0x10614, 0x10bc0, 0x1116c, 0x11718],
    ),
    (
        "DIG<i>_DIG_FE_CNTL",
        &[0x1552c, 0x1592c, 0x15d2c, 0x1612c, 0x1652c],
    ),
    (
        "DIG<i>_DIG_BE_CNTL",
        &[0x155c0, 0x159c0, 0x15dc0, 0x161c0, 0x165c0],
    ),
    (
        "DIG<i>_DIG_BE_EN_CNTL",
        &[0x155c4, 0x159c4, 0x15dc4, 0x161c4, 0x165c4],
    ),
    (
        "DIG<i>_HDMI_CONTROL",
        &[0x1554c, 0x1594c, 0x15d4c, 0x1614c, 0x1654c],
    ),
    (
        "DP<i>_DP_LINK_CNTL",
        &[0x15720, 0x15b20, 0x15f20, 0x16320, 0x16720],
    ),
    (
        "DP<i>_DP_VID_STREAM_CNTL",
        &[0x15730, 0x15b30, 0x15f30, 0x16330, 0x16730],
    ),
    (
        "DP<i>_DP_PIXEL_FORMAT",
        &[0x15724, 0x15b24, 0x15f24, 0x16324, 0x16724],
    ),
    (
        "HPD<i>_DC_HPD_INT_STATUS",
        &[0x14f50, 0x14f70, 0x14f90, 0x14fb0, 0x14fd0],
    ),
    (
        "HPD<i>_DC_HPD_CONTROL",
        &[0x14f58, 0x14f78, 0x14f98, 0x14fb8, 0x14fd8],
    ),
    (
        "DP_AUX<i>_AUX_CONTROL",
        &[0x15040, 0x150b0, 0x15120, 0x15190, 0x15200],
    ),
    ("DCN_VM_FB_LOCATION_BASE", &[0x0e4d4]),
    ("DCN_VM_FB_LOCATION_TOP", &[0x0e4d8]),
    ("DCN_VM_FB_OFFSET", &[0x0e4dc]),
    ("HUBPREQ0_DCN_VM_SYSTEM_APERTURE_LOW_ADDR", &[0x0ebd4]),
    ("HUBPREQ0_DCN_VM_SYSTEM_APERTURE_HIGH_ADDR", &[0x0ebd8]),
    ("DMCUB_CNTL", &[0x0dad8]),
    ("DMCUB_SCRATCH0", &[0x0da8c]),
    ("DC_GPIO_HPD_A", &[0x176d4]),
    ("DC_GPIO_HPD_Y", &[0x176dc]),
    ("DMCUB_SCRATCH14", &[0x0dac4]),
    ("DMCUB_INBOX1_BASE_ADDRESS", &[0x0da50]),
    ("DMCUB_INBOX1_SIZE", &[0x0da54]),
    ("DMCUB_INBOX1_WPTR", &[0x0da58]),
    ("DMCUB_INBOX1_RPTR", &[0x0da5c]),
];
