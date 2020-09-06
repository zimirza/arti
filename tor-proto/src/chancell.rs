//! Messages sent over Tor channels
//!
//! A 'channel' is a direct connection between a tor client and a
//! relay, or between two relays.  Current channels all use TLS.
//!
//! This module implements the "cell" type, which is the encoding for
//! data sent over a channel.  It also encodes and decodes various
//! channel messages, which are the types of data conveyed over a
//! channel.
#![allow(missing_docs)]

pub mod codec;
pub mod msg;
use bytes;
use caret::caret_int;

/// The amount of data sent in a fixed-length cell.
///
/// Historically, this was set at 509 bytes so that cells would be
/// 512 bytes long once commands and circuit IDs were added.  But now
/// circuit IDs are longer, so cells are 514 bytes.
pub const CELL_DATA_LEN: usize = 509;

/// Channel-local identifier for a circuit.
///
/// A circuit ID can be 2 or 4 bytes long; on modern versions of the Tor
/// protocol, it's 4 bytes long.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct CircID(u32);

impl From<u32> for CircID {
    fn from(item: u32) -> Self {
        Self(item)
    }
}
impl Into<u32> for CircID {
    fn into(self) -> u32 {
        self.0
    }
}

caret_int! {
    /// A ChanCmd is the type of a channel cell.  The value of the ChanCmd
    /// indicates the meaning of the cell, and (possibly) its length.
    pub struct ChanCmd(u8) {
        /// A fixed-length cell that will be dropped.
        PADDING = 0,
        /// Create a new circuit (obsolete format)
        CREATE = 1,
        /// Finish circuit-creation handshake (obsolete format)
        CREATED = 2,
        /// Relay cell, transmitted over a circuit.
        RELAY = 3,
        /// Destroy a circuit
        DESTROY = 4,
        /// Create a new circuit (no public-key)
        CREATE_FAST = 5,
        /// Finish a circuit-creation handshake (no public-key)
        CREATED_FAST = 6,
        // note gap in numbering: 7 is grouped with the variable-length cells
        /// Finish a channel handshake with time and address information
        NETINFO = 8,
        /// Relay cellm transmitted over a circuit.  Limited.
        RELAY_EARLY = 9,
        /// Create a new circuit (current format)
        CREATE2 = 10,
        /// Finish a circuit-creation handshake (current format)
        CREATED2 = 11,
        /// Adjust channel-padding settings
        PADDING_NEGOTIATE = 12,

        /// Variable-length cell, despite its number: negotiate versions
        VERSIONS = 7,
        /// Variable-length channel-padding cell
        VPADDING = 128,
        /// Provide additional certificates beyond those given in the TLS
        /// handshake
        CERTS = 129,
        /// Challenge material used in relay-to-relay handshake.
        AUTH_CHALLENGE = 130,
        /// Response material used in relay-to-relay handshake.
        AUTHENTICATE = 131,
        /// Indicates client permission to use relay.  Not currently used.
        AUTHORIZE = 132,
    }
}

impl ChanCmd {
    /// Return true if this cell uses the variable-length format.
    pub fn is_var_cell(self) -> bool {
        // Version 1 of the channel protocol had no variable-length
        // cells, but that's obsolete.  In version 2, only the VERSIONS
        // cell was variable-length.
        self == ChanCmd::VERSIONS || self.0 >= 128u8
    }
    pub fn is_recognized(self) -> bool {
        match self {
            ChanCmd::PADDING
            | ChanCmd::NETINFO
            | ChanCmd::PADDING_NEGOTIATE
            | ChanCmd::VERSIONS
            | ChanCmd::VPADDING
            | ChanCmd::CERTS
            | ChanCmd::AUTH_CHALLENGE
            | ChanCmd::AUTHENTICATE
            | ChanCmd::CREATE
            | ChanCmd::CREATED
            | ChanCmd::RELAY
            | ChanCmd::DESTROY
            | ChanCmd::CREATE_FAST
            | ChanCmd::CREATED_FAST
            | ChanCmd::RELAY_EARLY
            | ChanCmd::CREATE2
            | ChanCmd::CREATED2 => true,
            _ => false,
        }
    }
    pub fn allows_circid(self) -> bool {
        match self {
            ChanCmd::PADDING
            | ChanCmd::NETINFO
            | ChanCmd::PADDING_NEGOTIATE
            | ChanCmd::VERSIONS
            | ChanCmd::VPADDING
            | ChanCmd::CERTS
            | ChanCmd::AUTH_CHALLENGE
            | ChanCmd::AUTHENTICATE => false,
            ChanCmd::CREATE
            | ChanCmd::CREATED
            | ChanCmd::RELAY
            | ChanCmd::DESTROY
            | ChanCmd::CREATE_FAST
            | ChanCmd::CREATED_FAST
            | ChanCmd::RELAY_EARLY
            | ChanCmd::CREATE2
            | ChanCmd::CREATED2 => true,
            _ => true,
        }
    }
}

/// A single cell extracted from, or encodeable onto, a channel.
#[derive(Clone, Debug)]
pub struct ChanCell {
    circ: CircID,
    cmd: ChanCmd,
    body: bytes::Bytes,
}

impl ChanCell {
    /// Return the cell's circuit ID.
    pub fn get_circid(&self) -> CircID {
        self.circ
    }
    /// Return the cell's channel ID
    pub fn get_cmd(&self) -> ChanCmd {
        self.cmd
    }
    /// Return the body of this cell.
    pub fn get_body(&self) -> &bytes::Bytes {
        &self.body
    }
}
