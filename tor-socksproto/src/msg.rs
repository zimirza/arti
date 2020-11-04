//! Structures that represent SOCKS messages

use crate::{Error, Result};

use caret::caret_int;
use std::fmt;
use std::net::IpAddr;

/// A completed SOCKS request, as negotiated on a SOCKS connection.
///
/// Once this request is done, we know where to connect.  Don't
/// discard this object immediately: Use it to report success or
/// failure.
#[derive(Clone, Debug)]
pub struct SocksRequest {
    /// Negotiated SOCKS protocol version. This will be 4 or 5.
    version: u8,
    /// The command requested by the SOCKS client.
    cmd: SocksCmd,
    /// The target address.
    addr: SocksAddr,
    /// The target port.
    port: u16,
    /// Authentication information.
    ///
    /// (Tor doesn't believe in SOCKS authentication, since it cannot
    /// possibly secure.  Instead, we use it for circuit isolation.)
    auth: SocksAuth,
}

/// An address sent or received as part of a SOCKS handshake
#[derive(Clone, Debug)]
pub enum SocksAddr {
    /// A regular DNS hostname
    Hostname(String),
    /// An IP address.  (Tor doesn't like to receive these during SOCKS
    /// handshakes, since they usually indicate that the hostname lookup
    /// happened somewhere else.)
    Ip(IpAddr),
}

/// Provided authentication from a SOCKS handshake
#[derive(Clone, Debug, PartialEq)]
pub enum SocksAuth {
    /// No authentication was provided
    NoAuth,
    /// Socks4 authentication (a string) was provided.
    Socks4(Vec<u8>),
    /// Socks5 username/password authentication was provided.
    Username(Vec<u8>, Vec<u8>),
}

caret_int! {
    /// Command from the socks client telling us what to do.
    pub struct SocksCmd(u8) {
        /// Connect to a remote TCP address:port.
        CONNECT = 1,
        /// Not supported in Tor.
        BIND = 2,
        /// Not supported in Tor.
        UDP_ASSOCIATE = 3,

        /// Lookup a hostname, return an IP address. (Tor only.)
        RESOLVE = 0xF0,
        /// Lookup an IP address, return a hostname. (Tor only.)
        RESOLVE_PTR = 0xF1,
    }
}

caret_int! {
    /// Possible reply status values from a SOCKS5 handshake.
    ///
    /// Note that the documentation for these values is kind of scant,
    /// and is limited to what the RFC says.  Note also that SOCKS4
    /// only represents success and failure.
    pub struct SocksStatus(u8) {
        /// RFC 1928: "succeeded"
        SUCCEEDED = 0x00,
        /// RFC 1928: "general SOCKS server failure"
        GENERAL_FAILURE = 0x01,
        /// RFC 1928: "connection not allowable by ruleset"
        ///
        /// (This is the only occurrence of 'ruleset' or even 'rule'
        /// in RFC 1928.)
        NOT_ALLOWED = 0x02,
        /// RFC 1928: "Network unreachable"
        NETWORK_UNREACHABLE = 0x03,
        /// RFC 1928: "Host unreachable"
        HOST_UNREACHABLE = 0x04,
        /// RFC 1928: "Connection refused"
        CONNECTION_REFUSED = 0x05,
        /// RFC 1928: "TTL expired"
        ///
        /// (This is the only occurrence of 'TTL' in RFC 1928.)
        TTL_EXPIRED = 0x06,
        /// RFC 1929: "Command not supported"
        COMMAND_NOT_SUPPORTED = 0x07,
        /// RFC 1929: "Address type not supported"
        ADDRTYPE_NOT_SUPPORTED = 0x08,
    }
}

impl SocksCmd {
    /// Return true if this is a supported command.
    fn recognized(self) -> bool {
        match self {
            SocksCmd::CONNECT => true,
            SocksCmd::RESOLVE => true,
            SocksCmd::RESOLVE_PTR => true,
            _ => false,
        }
    }
}

impl SocksStatus {
    /// Convert this status into a value for use with SOCKS4 or SOCKS4a.
    pub(crate) fn into_socks4_status(self) -> u8 {
        match self {
            SocksStatus::SUCCEEDED => 0x5A,
            _ => 0x5B,
        }
    }
}

impl SocksRequest {
    /// Create a SocksRequest with a given set of fields.
    ///
    /// Return an error if the inputs aren't supported or valid.
    pub(crate) fn new(
        version: u8,
        cmd: SocksCmd,
        addr: SocksAddr,
        port: u16,
        auth: SocksAuth,
    ) -> Result<Self> {
        match version {
            4 | 5 => {}
            _ => {
                return Err(Error::NoSupport);
            }
        }
        if !cmd.recognized() {
            return Err(Error::NoSupport);
        }
        if port == 0 {
            return Err(Error::Syntax);
        }

        Ok(SocksRequest {
            version,
            cmd,
            addr,
            port,
            auth,
        })
    }

    /// Return the negotiated version (4 or 5).
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Return the command that the client requested.
    pub fn command(&self) -> SocksCmd {
        self.cmd
    }

    /// Return the 'authentication' information from this request.
    pub fn auth(&self) -> &SocksAuth {
        &self.auth
    }

    /// Return the requested port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Return the requested address.
    pub fn addr(&self) -> &SocksAddr {
        &self.addr
    }
}

impl fmt::Display for SocksAddr {
    /// Format a string (a hostname or IP address) corresponding to this
    /// SocksAddr.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SocksAddr::Ip(a) => write!(f, "{}", a),
            SocksAddr::Hostname(h) => write!(f, "{}", h),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn display_sa() {
        let a = SocksAddr::Ip(IpAddr::V4("127.0.0.1".parse().unwrap()));
        assert_eq!(a.to_string(), "127.0.0.1");

        let a = SocksAddr::Ip(IpAddr::V6("f00::9999".parse().unwrap()));
        assert_eq!(a.to_string(), "f00::9999");

        let a = SocksAddr::Hostname("www.torproject.org".into());
        assert_eq!(a.to_string(), "www.torproject.org");
    }
}
