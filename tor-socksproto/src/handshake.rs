//! Types to implement the SOCKS handshake.

use crate::msg::{SocksAddr, SocksAuth, SocksCmd, SocksRequest, SocksStatus};
use crate::{Error, Result};

use tor_bytes::Error as BytesError;
use tor_bytes::Result as BytesResult;
use tor_bytes::{Readable, Reader, Writeable, Writer};

use std::net::IpAddr;

/// An ongoing SOCKS handshake.
///
/// To perform a handshake, call the [SocksHandshake::handshake]
/// method repeatedly with new inputs, until the resulting [Action]
/// has `finished` set to true.
#[derive(Clone, Debug)]
pub struct SocksHandshake {
    /// Current state of the handshake. Each completed message
    /// advances the state.
    state: State,
    /// SOCKS5 authentication that has been received (but not yet put
    /// in a SocksRequest object.)
    socks5_auth: Option<SocksAuth>,
    /// Completed SOCKS handshake.
    handshake: Option<SocksRequest>,
}

/// Possible state for a Socks connection.
///
/// Each completed message advances the state.
#[derive(Clone, Debug, Copy, PartialEq)]
enum State {
    /// Starting state: no messages have been handled yet.
    Initial,
    /// SOCKS5: we've negotiated Username/Password authentication, and
    /// are waiting for the client to send it.
    Socks5Username,
    /// SOCKS5: we've finished the authentication (if any), and
    /// we're waiting for the actual request.
    Socks5Wait,
    /// Ending (successful) state: the client has sent all its messages.
    ///
    /// (Note that we still need to send a reply.)
    Done,
}

/// An action to take in response to a SOCKS handshake message.
#[derive(Clone, Debug)]
pub struct Action {
    /// If nonzero, this many bytes should be drained from the
    /// client's inputs.
    pub drain: usize,
    /// If nonempty, this reply should be sent to the client.
    pub reply: Vec<u8>,
    /// If true, then this handshake is over, either successfully or not.
    pub finished: bool,
}

impl SocksHandshake {
    /// Construct a new SocksHandshake in its initial state
    pub fn new() -> Self {
        SocksHandshake {
            state: State::Initial,
            socks5_auth: None,
            handshake: None,
        }
    }

    /// Try to advance a SocksHandshake, given some client input in
    /// `input`.
    ///
    /// If there isn't enough input, gives [Error::Truncated].  Other
    /// errors indicate a failure.
    ///
    /// On success, return an Action describing what to tell the client,
    /// and how much of its input to consume.
    pub fn handshake(&mut self, input: &[u8]) -> Result<Action> {
        if input.is_empty() {
            return Err(Error::Truncated);
        }
        match (self.state, input[0]) {
            (State::Initial, 4) => self.s4(input),
            (State::Initial, 5) => self.s5_initial(input),
            (State::Initial, v) => Err(Error::BadProtocol(v)),
            (State::Socks5Username, 1) => self.s5_uname(input),
            (State::Socks5Wait, 5) => self.s5(input),
            (State::Done, _) => Err(Error::AlreadyFinished),
            (_, _) => Err(Error::Syntax),
        }
    }

    /// Complete a socks4 or socks4a handshake.
    fn s4(&mut self, input: &[u8]) -> Result<Action> {
        let mut r = Reader::from_slice(input);
        let version = r.take_u8()?;
        assert_eq!(version, 4);

        let cmd: SocksCmd = r.take_u8()?.into();
        let port = r.take_u16()?;
        let ip = r.take_u32()?;
        let username = r.take_until(0)?.into();
        let auth = SocksAuth::Socks4(username);

        let addr = if ip != 0 && (ip >> 8) == 0 {
            // Socks4a; a hotname is given.
            let hostname = r.take_until(0)?;
            let hostname = std::str::from_utf8(hostname)
                .map_err(|_| Error::Syntax)?
                .to_string();
            SocksAddr::Hostname(hostname)
        } else {
            let ip4: std::net::Ipv4Addr = ip.into();
            SocksAddr::Ip(ip4.into())
        };

        let request = SocksRequest::new(version, cmd, addr, port, auth)?;

        self.state = State::Done;
        self.handshake = Some(request);

        Ok(Action {
            drain: r.consumed(),
            reply: Vec::new(),
            finished: true,
        })
    }

    /// Socks5: initial handshake to negotiate authentication method.
    fn s5_initial(&mut self, input: &[u8]) -> Result<Action> {
        let mut r = Reader::from_slice(input);
        let version = r.take_u8()?;
        assert_eq!(version, 5);
        /// Constant for Username/Password-style authentication.
        /// (See RFC 1929)
        const USERNAME_PASSWORD: u8 = 0x02;
        /// Constant for "no authentication".
        const NO_AUTHENTICATION: u8 = 0x00;

        let nmethods = r.take_u8()?;
        let methods = r.take(nmethods as usize)?;

        // Prefer username/password, then none.
        let (next, reply) = if methods.contains(&USERNAME_PASSWORD) {
            (State::Socks5Username, [5, USERNAME_PASSWORD])
        } else if methods.contains(&NO_AUTHENTICATION) {
            self.socks5_auth = Some(SocksAuth::NoAuth);
            (State::Socks5Wait, [5, NO_AUTHENTICATION])
        } else {
            // In theory we should reply with "NO ACCEPTABLE METHODS".
            return Err(Error::NoSupport);
        };

        self.state = next;
        Ok(Action {
            drain: r.consumed(),
            reply: reply.into(),
            finished: false,
        })
    }

    /// Socks5: second step for username/password authentication.
    fn s5_uname(&mut self, input: &[u8]) -> Result<Action> {
        let mut r = Reader::from_slice(input);

        let ver = r.take_u8()?;
        if ver != 1 {
            return Err(Error::NoSupport);
        }

        let ulen = r.take_u8()?;
        let username = r.take(ulen as usize)?;
        let plen = r.take_u8()?;
        let passwd = r.take(plen as usize)?;

        self.socks5_auth = Some(SocksAuth::Username(username.into(), passwd.into()));
        self.state = State::Socks5Wait;
        Ok(Action {
            drain: r.consumed(),
            reply: vec![1, 0],
            finished: false,
        })
    }

    /// Socks5: final step, to receive client's request.
    fn s5(&mut self, input: &[u8]) -> Result<Action> {
        let mut r = Reader::from_slice(input);

        let version = r.take_u8()?;
        if version != 5 {
            return Err(Error::Syntax);
        }
        let cmd = r.take_u8()?.into();
        let _ignore = r.take_u8()?;
        let addr = r.extract()?;
        let port = r.take_u16()?;

        let auth = self.socks5_auth.take().unwrap();

        let request = SocksRequest::new(version, cmd, addr, port, auth)?;

        self.state = State::Done;
        self.handshake = Some(request);

        Ok(Action {
            drain: r.consumed(),
            reply: Vec::new(),
            finished: true,
        })
    }

    /// Return true if this handshake is finished.
    pub fn finished(&self) -> bool {
        self.state == State::Done
    }

    /// Consume this handshake's state; if it finished successfully,
    /// return a SocksRequest.
    pub fn into_request(self) -> Option<SocksRequest> {
        self.handshake
    }
}

impl Default for SocksHandshake {
    fn default() -> Self {
        Self::new()
    }
}

impl SocksRequest {
    /// Format a reply to this request, indicating success or failure.
    ///
    /// Note that an address should be provided only when the request
    /// was for a RESOLVE.
    pub fn reply(&self, status: SocksStatus, addr: Option<&SocksAddr>) -> Vec<u8> {
        match self.version() {
            4 => self.s4(status, addr),
            5 => self.s5(status, addr),
            _ => panic!(),
        }
    }

    /// Format a SOCKS4 reply.
    fn s4(&self, status: SocksStatus, addr: Option<&SocksAddr>) -> Vec<u8> {
        let mut w = Vec::new();
        w.write_u8(0);
        w.write_u8(status.into_socks4_status());
        match addr {
            Some(SocksAddr::Ip(IpAddr::V4(ip))) => {
                w.write_u16(self.port());
                w.write(ip);
            }
            _ => {
                w.write_u16(0);
                w.write_u32(0);
            }
        }
        w
    }

    /// Format a SOCKS5 reply.
    pub fn s5(&self, status: SocksStatus, addr: Option<&SocksAddr>) -> Vec<u8> {
        let mut w = Vec::new();
        w.write_u8(5);
        w.write_u8(status.into());
        w.write_u8(0); // reserved.
        if let Some(a) = addr {
            w.write(a);
            w.write_u16(self.port());
        } else {
            // TODO: sometimes I think we want to answer with ::, not 0.0.0.0
            w.write(&SocksAddr::Ip(std::net::Ipv4Addr::UNSPECIFIED.into()));
            w.write_u16(0);
        }
        w
    }
}

impl Readable for SocksAddr {
    fn take_from(r: &mut Reader<'_>) -> BytesResult<SocksAddr> {
        let atype = r.take_u8()?;
        match atype {
            1 => {
                let ip4: std::net::Ipv4Addr = r.extract()?;
                Ok(SocksAddr::Ip(ip4.into()))
            }
            3 => {
                let hlen = r.take_u8()?;
                let hostname = r.take(hlen as usize)?;
                let hostname = std::str::from_utf8(hostname)
                    .map_err(|_| BytesError::BadMessage("bad utf8 on hostname"))?
                    .to_string();
                Ok(SocksAddr::Hostname(hostname))
            }
            4 => {
                let ip6: std::net::Ipv6Addr = r.extract()?;
                Ok(SocksAddr::Ip(ip6.into()))
            }
            _ => Err(BytesError::BadMessage("unrecognized address type.")),
        }
    }
}

impl Writeable for SocksAddr {
    fn write_onto<W: Writer + ?Sized>(&self, w: &mut W) {
        match self {
            SocksAddr::Ip(IpAddr::V4(ip)) => {
                w.write_u8(1);
                w.write(ip);
            }
            SocksAddr::Ip(IpAddr::V6(ip)) => {
                w.write_u8(4);
                w.write(ip);
            }
            SocksAddr::Hostname(h) => {
                assert!(h.len() < 256); // XXXX make sure we catch this earlier!
                let hlen = h.len() as u8;
                w.write_u8(3);
                w.write_u8(hlen);
                w.write(h.as_bytes());
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use hex_literal::hex;

    #[test]
    fn socks4_good() {
        let mut h = SocksHandshake::new();
        let a = h.handshake(&hex!("04 01 0050 CB007107 00")[..]).unwrap();
        assert!(a.finished);
        assert!(h.finished());
        assert_eq!(a.drain, 9);
        assert!(a.reply.is_empty()); // no reply -- waiting to see how it goes

        let req = h.into_request().unwrap();
        assert_eq!(req.port(), 80);
        assert_eq!(req.addr().to_string(), "203.0.113.7");
        assert_eq!(req.command(), SocksCmd::CONNECT);

        assert_eq!(
            req.reply(
                SocksStatus::GENERAL_FAILURE,
                Some(&SocksAddr::Ip("127.0.0.1".parse().unwrap()))
            ),
            hex!("00 5B 0050 7f000001")
        );
    }

    #[test]
    fn socks4a_good() {
        let mut h = SocksHandshake::new();
        let msg = hex!(
            "04 01 01BB 00000001 73776f72646669736800
                        7777772e6578616d706c652e636f6d00 99"
        );
        let a = h.handshake(&msg[..]).unwrap();
        assert!(a.finished);
        assert!(h.finished());
        assert_eq!(a.drain, msg.len() - 1);
        assert!(a.reply.is_empty()); // no reply -- waiting to see how it goes

        let req = h.into_request().unwrap();
        assert_eq!(req.port(), 443);
        assert_eq!(req.addr().to_string(), "www.example.com");
        assert_eq!(req.auth(), &SocksAuth::Socks4(b"swordfish".to_vec()));
        assert_eq!(req.command(), SocksCmd::CONNECT);

        assert_eq!(
            req.reply(SocksStatus::SUCCEEDED, None),
            hex!("00 5A 0000 00000000")
        );
    }

    #[test]
    fn socks5_init_noauth() {
        let mut h = SocksHandshake::new();
        let a = h.handshake(&hex!("05 01 00")[..]).unwrap();
        assert_eq!(a.finished, false);
        assert_eq!(a.drain, 3);
        assert_eq!(a.reply, &[5, 0]);
        assert_eq!(h.state, State::Socks5Wait);
    }

    #[test]
    fn socks5_init_username() {
        let mut h = SocksHandshake::new();
        let a = h.handshake(&hex!("05 04 00023031")[..]).unwrap();
        assert_eq!(a.finished, false);
        assert_eq!(a.drain, 6);
        assert_eq!(a.reply, &[5, 2]);
        assert_eq!(h.state, State::Socks5Username);
    }

    #[test]
    fn socks5_init_nothing_works() {
        let mut h = SocksHandshake::new();
        let a = h.handshake(&hex!("05 02 9988")[..]);
        dbg!(&a);
        assert!(matches!(a, Err(Error::NoSupport)));
    }

    #[test]
    fn socks5_username_ok() {
        let mut h = SocksHandshake::new();
        let _a = h.handshake(&hex!("05 02 9902")).unwrap();
        let a = h
            .handshake(&hex!("01 08 5761677374616666 09 24776f726466693568"))
            .unwrap();
        assert_eq!(a.drain, 20);
        assert_eq!(a.reply, &[1, 0]);
        assert_eq!(h.state, State::Socks5Wait);
        assert_eq!(
            h.socks5_auth.unwrap(),
            // _Horse Feathers_, 1932
            SocksAuth::Username(b"Wagstaff".to_vec(), b"$wordfi5h".to_vec())
        );
    }

    #[test]
    fn socks5_request_ok_ipv4() {
        let mut h = SocksHandshake::new();
        let _a = h.handshake(&hex!("05 01 00")).unwrap();
        let a = h.handshake(&hex!("05 01 00 01 7f000007 1f90")).unwrap();
        assert_eq!(a.drain, 10);
        assert!(a.finished);
        assert!(a.reply.is_empty());
        assert_eq!(h.state, State::Done);

        let req = h.into_request().unwrap();
        assert_eq!(req.version(), 5);
        assert_eq!(req.command(), SocksCmd::CONNECT);
        assert_eq!(req.addr().to_string(), "127.0.0.7");
        assert_eq!(req.port(), 8080);
        assert_eq!(req.auth(), &SocksAuth::NoAuth);

        assert_eq!(
            req.reply(
                SocksStatus::HOST_UNREACHABLE,
                Some(&SocksAddr::Hostname("foo.example.com".into()))
            ),
            hex!("05 04 00 03 0f 666f6f2e6578616d706c652e636f6d 1f90")
        );
    }

    #[test]
    fn socks5_request_ok_ipv6() {
        let mut h = SocksHandshake::new();
        let _a = h.handshake(&hex!("05 01 00")).unwrap();
        let a = h
            .handshake(&hex!(
                "05 01 00 04 f000 0000 0000 0000 0000 0000 0000 ff11 1f90"
            ))
            .unwrap();
        assert_eq!(a.drain, 22);
        assert!(a.finished);
        assert!(a.reply.is_empty());
        assert_eq!(h.state, State::Done);

        let req = h.into_request().unwrap();
        assert_eq!(req.version(), 5);
        assert_eq!(req.command(), SocksCmd::CONNECT);
        assert_eq!(req.addr().to_string(), "f000::ff11");
        assert_eq!(req.port(), 8080);
        assert_eq!(req.auth(), &SocksAuth::NoAuth);

        assert_eq!(
            req.reply(SocksStatus::GENERAL_FAILURE, Some(req.addr())),
            hex!("05 01 00 04 f000 0000 0000 0000 0000 0000 0000 ff11 1f90")
        );
    }

    #[test]
    fn socks5_request_ok_hostname() {
        let mut h = SocksHandshake::new();
        let _a = h.handshake(&hex!("05 01 00")).unwrap();
        let a = h
            .handshake(&hex!("05 01 00 03 0f 666f6f2e6578616d706c652e636f6d 1f90"))
            .unwrap();
        assert_eq!(a.drain, 22);
        assert!(a.finished);
        assert!(a.reply.is_empty());
        assert_eq!(h.state, State::Done);

        let req = h.into_request().unwrap();
        assert_eq!(req.version(), 5);
        assert_eq!(req.command(), SocksCmd::CONNECT);
        assert_eq!(req.addr().to_string(), "foo.example.com");
        assert_eq!(req.port(), 8080);
        assert_eq!(req.auth(), &SocksAuth::NoAuth);

        assert_eq!(
            req.reply(SocksStatus::SUCCEEDED, None),
            hex!("05 00 00 01 00000000 0000")
        );
    }
}
