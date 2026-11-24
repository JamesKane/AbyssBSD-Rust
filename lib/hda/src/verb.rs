// SPDX-License-Identifier: BSD-2-Clause
//! Verbs and the parameters they read (specification section 7).

#![allow(
    clippy::cast_possible_truncation,
    reason = "each cast takes one field of a 32-bit word"
)]

/// A response that never came, or came all ones (`HDA_INVALID`).
pub const INVALID: u32 = 0xffff_ffff;

/// A verb to codec `cad`, node `nid`, with a 12-bit verb and an 8-bit
/// payload.
#[must_use]
pub const fn verb12(cad: u8, nid: u8, verb: u16, payload: u8) -> u32 {
    (cad as u32) << 28 | (nid as u32) << 20 | ((verb as u32) & 0xfff) << 8 | payload as u32
}

/// A verb with a 4-bit verb and a 16-bit payload.
#[must_use]
pub const fn verb4(cad: u8, nid: u8, verb: u8, payload: u16) -> u32 {
    (cad as u32) << 28 | (nid as u32) << 20 | ((verb as u32) & 0xf) << 16 | payload as u32
}

/// Get Parameter (`HDA_CMD_GET_PARAMETER`).
#[must_use]
pub const fn get_parameter(cad: u8, nid: u8, param: u8) -> u32 {
    verb12(cad, nid, 0xf00, param)
}

/// Get Connection List Entry, from entry `index`.
#[must_use]
pub const fn get_conn_list_entry(cad: u8, nid: u8, index: u8) -> u32 {
    verb12(cad, nid, 0xf02, index)
}

/// Get Subsystem ID.
#[must_use]
pub const fn get_subsystem_id(cad: u8, nid: u8) -> u32 {
    verb12(cad, nid, 0xf20, 0)
}

/// Get Pin Configuration Default.
#[must_use]
pub const fn get_config_default(cad: u8, nid: u8) -> u32 {
    verb12(cad, nid, 0xf1c, 0)
}

/// Set Power State.
#[must_use]
pub const fn set_power_state(cad: u8, nid: u8, state: u8) -> u32 {
    verb12(cad, nid, 0x705, state)
}

/// The D0 power state: fully on.
pub const POWER_D0: u8 = 0;
/// The D3 power state: off.
pub const POWER_D3: u8 = 3;

/// Parameter: vendor and device ID.
pub const PARAM_VENDOR_ID: u8 = 0x00;
/// Parameter: revision and stepping.
pub const PARAM_REVISION_ID: u8 = 0x02;
/// Parameter: the subordinate nodes.
pub const PARAM_SUB_NODE_COUNT: u8 = 0x04;
/// Parameter: a function group's type.
pub const PARAM_FCT_GRP_TYPE: u8 = 0x05;
/// Parameter: an audio widget's capabilities.
pub const PARAM_AUDIO_WIDGET_CAP: u8 = 0x09;
/// Parameter: the length of a connection list.
pub const PARAM_CONN_LIST_LEN: u8 = 0x0e;

/// A `SUB_NODE_COUNT` response: the first node's number.
#[must_use]
pub const fn sub_node_start(r: u32) -> u8 {
    (r >> 16) as u8
}

/// A `SUB_NODE_COUNT` response: how many nodes.
#[must_use]
pub const fn sub_node_total(r: u32) -> u8 {
    r as u8
}

/// A `VENDOR_ID` response: the vendor.
#[must_use]
pub const fn vendor_of(r: u32) -> u16 {
    (r >> 16) as u16
}

/// A `VENDOR_ID` response: the device.
#[must_use]
pub const fn device_of(r: u32) -> u16 {
    r as u16
}

/// A `FCT_GRP_TYPE` response: the node type.
#[must_use]
pub const fn fct_grp_type(r: u32) -> u8 {
    r as u8
}

/// The audio function group's node type.
pub const FCT_GRP_AUDIO: u8 = 0x01;
/// The modem function group's node type.
pub const FCT_GRP_MODEM: u8 = 0x02;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_encode_as_the_specification_lays_them_out() {
        // Get Parameter, vendor ID, of the root node of codec 2.
        assert_eq!(get_parameter(2, 0, PARAM_VENDOR_ID), 0x200f_0000);
        // Sub node count of node 1 on codec 0.
        assert_eq!(get_parameter(0, 1, PARAM_SUB_NODE_COUNT), 0x001f_0004);
        // Connection list from entry 4 of node 0x14.
        assert_eq!(get_conn_list_entry(0, 0x14, 4), 0x014f_0204);
        assert_eq!(set_power_state(0, 1, POWER_D3), 0x0017_0503);
        // A 4-bit verb (Set Amp Gain/Mute is 3h) carries 16 bits.
        assert_eq!(verb4(0, 2, 3, 0xb035), 0x0023_b035);
    }

    #[test]
    fn responses_split_into_their_fields() {
        assert_eq!(sub_node_start(0x0001_0002), 1);
        assert_eq!(sub_node_total(0x0001_0002), 2);
        assert_eq!(vendor_of(0x1af4_0022), 0x1af4);
        assert_eq!(device_of(0x1af4_0022), 0x0022);
        // Bit 8 (unsolicited capable) is not part of the type.
        assert_eq!(fct_grp_type(0x0000_0101), FCT_GRP_AUDIO);
    }
}
