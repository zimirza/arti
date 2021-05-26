//! Parsing implementation for networkstatus documents.
//!
//! In Tor, a networkstatus documents describes a complete view of the
//! relays in the network: how many there are, how to contact them,
//! and so forth.
//!
//! A networkstatus document can either be a "votes" -- an authority's
//! view of the network, used as input to the voting process -- or a
//! "consensus" -- a combined view of the network based on multiple
//! authorities' votes, and signed by multiple authorities.
//!
//! A consensus document can itself come in two different flavors: a
//! "ns"-flavored consensus has references to router descriptors, and
//! a "microdesc"-flavored consensus has references to
//! microdescriptors.
//!
//! To keep an up-to-date view of the network, clients download
//! microdescriptor-flavored consensuses periodically, and then
//! download whatever microdescriptors the consensus lists that the
//! client doesn't already have.
//!
//! For full information about the network status format, see
//! [dir-spec.txt](https://spec.torproject.org/dir-spec).
//!
//! # Limitations
//!
//! NOTE: The consensus format has changes time, using a
//! "consensus-method" mechanism.  This module is does not yet handle all
//! all historical consensus-methods.
//!
//! NOTE: This module _does_ parse some fields that are not in current
//! use, like relay nicknames, and the "published" times on
//! microdescriptors. We should probably decide whether we actually
//! want to do this.
//!
//! TODO: This module doesn't implement vote parsing at all yet.
//!
//! TODO: This module doesn't implement ns-flavored consensuses.
//!
//! TODO: More testing is needed!
//!
//! TODO: There should be accessor functions for most of the fields here.
//! As with the other tor-netdoc types, I'm deferring those till I know what
//! they should be.

mod rs;

#[cfg(feature = "build_docs")]
mod build;

use crate::doc::authcert::{AuthCert, AuthCertKeyIds};
use crate::parse::keyword::Keyword;
use crate::parse::parser::{Section, SectionRules};
use crate::parse::tokenize::{Item, ItemResult, NetDocReader};
use crate::types::misc::*;
use crate::util::private::Sealed;
use crate::{Error, Pos, Result};
use std::collections::{HashMap, HashSet};
use std::{net, result, time};
use tor_protover::Protocols;

use bitflags::bitflags;
use digest::Digest;
use once_cell::sync::Lazy;
use tor_checkable::{timed::TimerangeBound, ExternallySigned};
use tor_llcrypto as ll;
use tor_llcrypto::pk::rsa::RsaIdentity;

use serde::{Deserialize, Deserializer};

#[cfg(feature = "build_docs")]
pub use build::ConsensusBuilder;
#[cfg(feature = "build_docs")]
pub use rs::build::RouterStatusBuilder;

pub use rs::MdConsensusRouterStatus;
pub use rs::NsConsensusRouterStatus;

/// The lifetime of a networkstatus document.
///
/// In a consensus, this type describes when the consensus may safely
/// be used.  In a vote, this type describes the proposed lifetime for a
/// consensus.
#[derive(Clone, Debug)]
pub struct Lifetime {
    /// Time at which the document becomes valid
    valid_after: time::SystemTime,
    /// Time after which there is expected to be a better version
    /// of this consensus
    fresh_until: time::SystemTime,
    /// Time after which this consensus is expired.
    ///
    /// (In practice, Tor clients will keep using documents for a while
    /// after this expiration time, if no better one can be found.)
    valid_until: time::SystemTime,
}

impl Lifetime {
    /// Construct a new Lifetime.
    pub fn new(
        valid_after: time::SystemTime,
        fresh_until: time::SystemTime,
        valid_until: time::SystemTime,
    ) -> Result<Self> {
        if valid_after < fresh_until && fresh_until < valid_until {
            Ok(Lifetime {
                valid_after,
                fresh_until,
                valid_until,
            })
        } else {
            Err(Error::InvalidLifetime)
        }
    }
    /// Return time when this consensus first becomes valid.
    ///
    /// (You might see a consensus a little while before this time,
    /// since voting tries to finish up before the.)
    pub fn valid_after(&self) -> time::SystemTime {
        self.valid_after
    }
    /// Return time when this consensus is no longer fresh.
    ///
    /// You can use the consensus after this time, but there is (or is
    /// supposed to be) a better one by this point.
    pub fn fresh_until(&self) -> time::SystemTime {
        self.fresh_until
    }
    /// Return the time when this consensus is no longer valid.
    ///
    /// You should try to get a better consensus after this time,
    /// though it's okay to keep using this one if no more recent one
    /// can be found.
    pub fn valid_until(&self) -> time::SystemTime {
        self.valid_until
    }
}

/// A set of named network parameters.
///
/// These are used to describe current settings for the Tor network,
/// current weighting parameters for path selection, and so on.  They're
/// encoded with a space-separated K=V format.
#[derive(Debug, Clone, Default)]
pub struct NetParams<T> {
    /// Map from keys to values.
    params: HashMap<String, T>,
}

impl<T> NetParams<T> {
    /// Create a new empty list of NetParams.
    #[allow(unused)]
    pub(crate) fn new() -> Self {
        NetParams {
            params: HashMap::new(),
        }
    }
    /// Retrieve a given network parameter, if it is present.
    pub fn get<A: AsRef<str>>(&self, v: A) -> Option<&T> {
        self.params.get(v.as_ref())
    }
    /// Return an iterator over all key value pares in an arbitrary order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &T)> {
        self.params.iter()
    }
    /// Set or replace the value of a network parameter.
    pub fn set(&mut self, k: String, v: T) {
        self.params.insert(k, v);
    }
}

impl<'de, T> Deserialize<'de> for NetParams<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let params = HashMap::deserialize(deserializer)?;
        Ok(NetParams { params })
    }
}

/// A list of subprotocol versions that implementors should/must provide.
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct ProtoStatus {
    /// Set of protocols that are recommended; if we're missing a protocol
    /// in this list we should warn the user.
    recommended: Protocols,
    /// Set of protocols that are required; if we're missing a protocol
    /// in this list we should refuse to start.
    required: Protocols,
}

/// A recognized 'flavor' of consensus document.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[non_exhaustive]
pub enum ConsensusFlavor {
    /// A "microdesc"-flavored consensus.  This is the one that
    /// clients and relays use today.
    Microdesc,
    /// A "networkstatus"-flavored consensus.  It's used for
    /// historical and network-health purposes.  Instead of listing
    /// microdescriptor digests, it lists digests of full relay
    /// descriptors.
    Ns,
}

impl ConsensusFlavor {
    /// Return the name of this consensus flavor.
    pub fn name(&self) -> &'static str {
        match self {
            ConsensusFlavor::Ns => "ns",
            ConsensusFlavor::Microdesc => "microdesc",
        }
    }
    /// Try to find the flavor whose name is `name`.
    ///
    /// For historical reasons, an unnamed flavor indicates an "Ns"
    /// document.
    pub fn from_opt_name(name: Option<&str>) -> Result<Self> {
        match name {
            Some("microdesc") => Ok(ConsensusFlavor::Microdesc),
            Some("ns") | None => Ok(ConsensusFlavor::Ns),
            _ => Err(Error::BadDocumentType),
        }
    }
}

/// The signature of a single directory authority on a networkstatus document.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Signature {
    /// The name of the digest algorithm used to make the signature.
    ///
    /// Currently sha1 and sh256 are recognized.  Here we only support
    /// sha256.
    digestname: String,
    /// Fingerprints of the keys for the authority that made
    /// this signature.
    key_ids: AuthCertKeyIds,
    /// The signature itself.
    signature: Vec<u8>,
}

/// A collection of signatures that can be checked on a networkstatus document
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SignatureGroup {
    /// The sha256 of the document itself
    sha256: Option<[u8; 32]>,
    /// The sha1 of the document itself
    sha1: Option<[u8; 20]>,
    /// The signatures listed on the document.
    signatures: Vec<Signature>,
}

/// A shared-random value produced by the directory authorities.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct SharedRandVal {
    /// How many authorities revealed shares that contributed to this value.
    n_reveals: u8,
    /// The current random value.
    ///
    /// The properties of the secure shared-random system guarantee
    /// that this value isn't predictable before it first becomes
    /// live, and that a hostile party could not have forced it to
    /// have any more than a small number of possible random values.
    value: Vec<u8>,
}

/// Parts of the networkstatus header that are present in every networkstatus.
///
/// NOTE: this type is separate from the header parts that are only in
/// votes or only in consensuses, even though we don't implement votes yet.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct CommonHeader {
    /// What kind of consensus document is this?  Absent in votes and
    /// in ns-flavored consensuses.
    flavor: ConsensusFlavor,
    /// Over what time is this consensus valid?  (For votes, this is
    /// the time over which the voted-upon consensus should be valid.)
    lifetime: Lifetime,
    /// List of recommended Tor client versions.
    client_versions: Vec<String>,
    /// List of recommended Tor relay versions.
    relay_versions: Vec<String>,
    /// Lists of recommended and required subprotocol versions for clients
    client_protos: ProtoStatus,
    /// Lists of recommended and required subprotocol versions for relays
    relay_protos: ProtoStatus,
    /// Declared parameters for tunable settings about how to the
    /// network should operator. Some of these adjust timeouts and
    /// whatnot; some features things on and off.
    params: NetParams<i32>,
    /// How long in seconds should voters wait for votes and
    /// signatures (respectively) to propagate?
    voting_delay: Option<(u32, u32)>,
}

/// The header of a consensus networkstatus.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct ConsensusHeader {
    /// Header fields common to votes and consensuses
    hdr: CommonHeader,
    /// What "method" was used to produce this consensus?  (A
    /// consensus method is a version number used by authorities to
    /// upgrade the consensus algorithm.)
    consensus_method: u32,
    /// Global shared-random value for the previous shared-random period.
    shared_rand_prev: Option<SharedRandVal>,
    /// Global shared-random value for the current shared-random period.
    shared_rand_cur: Option<SharedRandVal>,
}

/// Description of an authority's identity and address.
///
/// (Corresponds to a dir-source line.)
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct DirSource {
    /// human-readable nickname for this authority.
    nickname: String,
    /// DOCDOC -- I forget.  Is this the identity fingerprint for the
    /// authority identity key, or for the identity key of the authority
    /// when it's runnign as a relay?
    identity: RsaIdentity,
    /// Address of the authority in string form.
    // XXXX why do we have this _and_ IP?
    address: String,
    /// IP address for the authority
    ip: net::IpAddr,
    /// HTTP directory port for this authoirty
    dir_port: u16,
    /// OR port for this authority.
    or_port: u16,
}

bitflags! {
    /// A set of recognized directory flags on a single relay.
    ///
    /// These flags come from a consensus directory document, and are
    /// used to describe what the authorities believe about the relay.
    /// If the document contained any flags that we _didn't_ recognize,
    /// they are not listed in this type.
    ///
    /// The bit values used to represent the flags have no meaning.
    pub struct RelayFlags: u16 {
        /// Is this a directory authority?
        const AUTHORITY = (1<<0);
        /// Is this relay marked as a bad exit?
        ///
        /// Bad exits can be used as intermediate relays, but not to
        /// deliver traffic.
        const BAD_EXIT = (1<<1);
        /// Is this relay marked as an exit for weighting purposes?
        const EXIT = (1<<2);
        /// Is this relay considered "fast" above a certain threshold?
        const FAST = (1<<3);
        /// Is this relay suitable for use as a guard relay?
        ///
        /// Clients choose their their initial relays from among the set
        /// of Guard relays.
        const GUARD = (1<<4);
        /// Does this relay participate on the hidden service directory
        /// ring?
        const HSDIR = (1<<5);
        /// If set, there is no consensus for the ed25519 key for this relay.
        const NO_ED_CONSENSUS = (1<<6);
        /// Is this relay considered "stable" enough for long-lived circuits?
        const STABLE = (1<<7);
        /// Set if the authorities are requesting a fresh descriptor for
        /// this relay.
        const STALE_DESC = (1<<8);
        /// Set if this relay is currently running.
        ///
        /// This flag can appear in votes, but in consensuses, every relay
        /// is assumed to be running.
        const RUNNING = (1<<9);
        /// Set if this relay is considered "valid" -- allowed to be on
        /// the network.
        ///
        /// This flag can appear in votes, but in consensuses, every relay
        /// is assumed to be valid.
        const VALID = (1<<10);
        /// Set if this relay supports a currently recognized version of the
        /// directory protocol.
        const V2DIR = (1<<11);
    }
}

/// Recognized weight fields on a single relay in a consensus
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum RelayWeight {
    // TODO SPEC: Document that these are u32 in dir-spec.txt
    /// An unmeasured weight for a relay.
    Unmeasured(u32),
    /// An measured weight for a relay.
    Measured(u32),
}

impl RelayWeight {
    /// Return true if this weight is the result of a successful measurement
    pub fn is_measured(&self) -> bool {
        matches!(self, RelayWeight::Measured(_))
    }
    /// Return true if this weight is nonzero
    pub fn is_nonzero(&self) -> bool {
        !matches!(self, RelayWeight::Unmeasured(0) | RelayWeight::Measured(0))
    }
}

/// All information about a single authority, as represented in a consensus
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct ConsensusVoterInfo {
    /// Contents of the dirsource line about an authority
    dir_source: DirSource,
    /// Human-readable contact information about the authority
    contact: String,
    /// Digest of the vote that the authority cast to contribute to
    /// this consensus.
    vote_digest: Vec<u8>,
}

/// The signed footer of a consensus netstatus.
#[allow(dead_code)]
#[derive(Debug, Clone)]
struct Footer {
    /// Weights to be applied to certain classes of relays when choosing
    /// for different roles.
    ///
    /// For example, we want to avoid choosing exits for non-exit
    /// roles when overall the proportion of exits is small.
    weights: NetParams<i32>,
}

/// Trait to parse a single relay as listed in a consensus document.
///
/// XXXX: I'd rather not have this trait be public, but I haven't yet
/// figured out how to make it private.
pub trait ParseRouterStatus: Sized + Sealed {
    /// Parse this object from a `Section` object containing its
    /// elements.
    fn from_section(sec: &Section<'_, NetstatusKwd>) -> Result<Self>;

    /// Return the networkstatus consensus flavor in which this
    /// routerstatus appears.
    fn flavor() -> ConsensusFlavor;
}

/// Represents a single relay as listed in a consensus document.
///
/// Not implementable outside of the `tor-netdoc` crate.
pub trait RouterStatus: Sealed {
    /// A digest of the document that's identified by this RouterStatus.
    type DocumentDigest: Clone;

    /// Return RSA identity for the relay described by this RouterStatus
    fn rsa_identity(&self) -> &RsaIdentity;

    /// Return the digest of the document identified by this
    /// routerstatus.
    fn doc_digest(&self) -> &Self::DocumentDigest;
}

/// A single microdescriptor consensus netstatus
///
/// TODO: This should possibly turn into a parameterized type, to represent
/// votes and ns consensuses.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Consensus<RS> {
    /// Part of the header shared by all consensus types.
    header: ConsensusHeader,
    /// List of voters whose votes contributed to this consensus.
    voters: Vec<ConsensusVoterInfo>,
    /// A list of routerstatus entries for the relays on the network,
    /// with one entry per relay.
    relays: Vec<RS>,
    /// Footer for the consensus object.
    footer: Footer,
}

/// A consensus document that lists relays along with their
/// microdescriptor documents.
pub type MdConsensus = Consensus<MdConsensusRouterStatus>;

/// An MdConsensus that has been parsed and checked for timeliness,
/// but not for signatures.
pub type UnvalidatedMdConsensus = UnvalidatedConsensus<MdConsensusRouterStatus>;

/// An MdConsensus that has been parsed but not checked for signatures
/// and timeliness.
pub type UncheckedMdConsensus = UncheckedConsensus<MdConsensusRouterStatus>;

/// A consensus document that lists relays along with their
/// router descriptor documents.
pub type NsConsensus = Consensus<NsConsensusRouterStatus>;

/// An NsConsensus that has been parsed and checked for timeliness,
/// but not for signatures.
pub type UnvalidatedNsConsensus = UnvalidatedConsensus<NsConsensusRouterStatus>;

/// An NsConsensus that has been parsed but not checked for signatures
/// and timeliness.
pub type UncheckedNsConsensus = UncheckedConsensus<NsConsensusRouterStatus>;

impl<RS> Consensus<RS> {
    /// Return the Lifetime for this consensus.
    pub fn lifetime(&self) -> &Lifetime {
        &self.header.hdr.lifetime
    }

    /// Return a slice of all the routerstatus entries in this consensus.
    pub fn relays(&self) -> &[RS] {
        &self.relays[..]
    }

    /// Return a mapping from keywords to integers representing how
    /// to weight different kinds of relays in different path positions.
    pub fn bandwidth_weights(&self) -> &NetParams<i32> {
        &self.footer.weights
    }

    /// Return the map of network parameters that this consensus advertises.
    pub fn params(&self) -> &NetParams<i32> {
        &self.header.hdr.params
    }
}

decl_keyword! {
    /// Keywords that can be used in votes and consensuses.
    // TODO: This is public because otherwise we can't use it in the
    // ParseRouterStatus crate.  But I'd rather find a way to make it
    // private.
    #[non_exhaustive]
    #[allow(missing_docs)]
    pub NetstatusKwd {
        // Header
        "network-status-version" => NETWORK_STATUS_VERSION,
        "vote-status" => VOTE_STATUS,
        "consensus-methods" => CONSENSUS_METHODS,
        "consensus-method" => CONSENSUS_METHOD,
        "published" => PUBLISHED,
        "valid-after" => VALID_AFTER,
        "fresh-until" => FRESH_UNTIL,
        "valid-until" => VALID_UNTIL,
        "voting-delay" => VOTING_DELAY,
        "client-versions" => CLIENT_VERSIONS,
        "server-versions" => SERVER_VERSIONS,
        "known-flags" => KNOWN_FLAGS,
        "flag-thresholds" => FLAG_THRESHOLDS,
        "recommended-client-protocols" => RECOMMENDED_CLIENT_PROTOCOLS,
        "required-client-protocols" => REQUIRED_CLIENT_PROTOCOLS,
        "recommended-relay-protocols" => RECOMMENDED_RELAY_PROTOCOLS,
        "required-relay-protocols" => REQUIRED_RELAY_PROTOCOLS,
        "params" => PARAMS,
        "bandwidth-file-headers" => BANDWIDTH_FILE_HEADERS,
        "bandwidth-file-digest" => BANDWIDTH_FILE_DIGEST,
        // "package" is now ignored.

        // header in consensus, voter section in vote?
        "shared-rand-previous-value" => SHARED_RAND_PREVIOUS_VALUE,
        "shared-rand-current-value" => SHARED_RAND_CURRENT_VALUE,

        // Voter section (both)
        "dir-source" => DIR_SOURCE,
        "contact" => CONTACT,

        // voter section (vote, but not consensus)
        "legacy-dir-key" => LEGACY_DIR_KEY,
        "shared-rand-participate" => SHARED_RAND_PARTICIPATE,
        "shared-rand-commit" => SHARED_RAND_COMMIT,

        // voter section (consensus, but not vote)
        "vote-digest" => VOTE_DIGEST,

        // voter cert beginning (but only the beginning)
        "dir-key-certificate-version" => DIR_KEY_CERTIFICATE_VERSION,

        // routerstatus
        "r" => RS_R,
        "a" => RS_A,
        "s" => RS_S,
        "v" => RS_V,
        "pr" => RS_PR,
        "w" => RS_W,
        "p" => RS_P,
        "m" => RS_M,
        "id" => RS_ID,

        // footer
        "directory-footer" => DIRECTORY_FOOTER,
        "bandwidth-weights" => BANDWIDTH_WEIGHTS,
        "directory-signature" => DIRECTORY_SIGNATURE,
    }
}

/// Shared parts of rules for all kinds of netstatus headers
static NS_HEADER_RULES_COMMON_: Lazy<SectionRules<NetstatusKwd>> = Lazy::new(|| {
    use NetstatusKwd::*;
    let mut rules = SectionRules::new();
    rules.add(NETWORK_STATUS_VERSION.rule().required().args(1..=2));
    rules.add(VOTE_STATUS.rule().required().args(1..));
    rules.add(VALID_AFTER.rule().required());
    rules.add(FRESH_UNTIL.rule().required());
    rules.add(VALID_UNTIL.rule().required());
    rules.add(VOTING_DELAY.rule().args(2..));
    rules.add(CLIENT_VERSIONS.rule());
    rules.add(SERVER_VERSIONS.rule());
    rules.add(KNOWN_FLAGS.rule().required());
    rules.add(RECOMMENDED_CLIENT_PROTOCOLS.rule().args(1..));
    rules.add(RECOMMENDED_RELAY_PROTOCOLS.rule().args(1..));
    rules.add(REQUIRED_CLIENT_PROTOCOLS.rule().args(1..));
    rules.add(REQUIRED_RELAY_PROTOCOLS.rule().args(1..));
    rules.add(PARAMS.rule());
    rules
});
/// Rules for parsing the header of a consensus.
static NS_HEADER_RULES_CONSENSUS: Lazy<SectionRules<NetstatusKwd>> = Lazy::new(|| {
    use NetstatusKwd::*;
    let mut rules = NS_HEADER_RULES_COMMON_.clone();
    rules.add(CONSENSUS_METHOD.rule().args(1..=1));
    rules.add(SHARED_RAND_PREVIOUS_VALUE.rule().args(2..));
    rules.add(SHARED_RAND_CURRENT_VALUE.rule().args(2..));
    rules.add(UNRECOGNIZED.rule().may_repeat().obj_optional());
    rules
});
/*
/// Rules for parsing the header of a vote.
static NS_HEADER_RULES_VOTE: SectionRules<NetstatusKwd> = {
    use NetstatusKwd::*;
    let mut rules = NS_HEADER_RULES_COMMON_.clone();
    rules.add(CONSENSUS_METHODS.rule().args(1..));
    rules.add(FLAG_THRESHOLDS.rule());
    rules.add(BANDWIDTH_FILE_HEADERS.rule());
    rules.add(BANDWIDTH_FILE_DIGEST.rule().args(1..));
    rules.add(UNRECOGNIZED.rule().may_repeat().obj_optional());
    rules
};
/// Rules for parsing a single voter's information in a vote.
static NS_VOTERINFO_RULES_VOTE: SectionRules<NetstatusKwd> = {
    use NetstatusKwd::*;
    let mut rules = SectionRules::new();
    rules.add(DIR_SOURCE.rule().required().args(6..));
    rules.add(CONTACT.rule().required());
    rules.add(LEGACY_DIR_KEY.rule().args(1..));
    rules.add(SHARED_RAND_PARTICIPATE.rule().no_args());
    rules.add(SHARED_RAND_COMMIT.rule().may_repeat().args(4..));
    rules.add(SHARED_RAND_PREVIOUS_VALUE.rule().args(2..));
    rules.add(SHARED_RAND_CURRENT_VALUE.rule().args(2..));
    // then comes an entire cert: When we implement vote parsing,
    // we should use the authcert code for handling that.
    rules.add(UNRECOGNIZED.rule().may_repeat().obj_optional());
    rules
};
 */
/// Rules for parsing a single voter's information in a consensus
static NS_VOTERINFO_RULES_CONSENSUS: Lazy<SectionRules<NetstatusKwd>> = Lazy::new(|| {
    use NetstatusKwd::*;
    let mut rules = SectionRules::new();
    rules.add(DIR_SOURCE.rule().required().args(6..));
    rules.add(CONTACT.rule().required());
    rules.add(VOTE_DIGEST.rule().required());
    rules.add(UNRECOGNIZED.rule().may_repeat().obj_optional());
    rules
});
/// Shared rules for parsing a single routerstatus
static NS_ROUTERSTATUS_RULES_COMMON_: Lazy<SectionRules<NetstatusKwd>> = Lazy::new(|| {
    use NetstatusKwd::*;
    let mut rules = SectionRules::new();
    rules.add(RS_A.rule().may_repeat().args(1..));
    rules.add(RS_S.rule().required());
    rules.add(RS_V.rule());
    rules.add(RS_PR.rule().required());
    rules.add(RS_W.rule());
    rules.add(RS_P.rule().args(2..));
    rules.add(UNRECOGNIZED.rule().may_repeat().obj_optional());
    rules
});

/// Rules for parsing a single routerstatus in an NS consensus
static NS_ROUTERSTATUS_RULES_NSCON: Lazy<SectionRules<NetstatusKwd>> = Lazy::new(|| {
    use NetstatusKwd::*;
    let mut rules = NS_ROUTERSTATUS_RULES_COMMON_.clone();
    rules.add(RS_R.rule().required().args(8..));
    rules
});

/*
/// Rules for parsing a single routerstatus in a vote
static NS_ROUTERSTATUS_RULES_VOTE: SectionRules<NetstatusKwd> = {
    use NetstatusKwd::*;
        let mut rules = NS_ROUTERSTATUS_RULES_COMMON_.clone();
        rules.add(RS_R.rule().required().args(8..));
        rules.add(RS_M.rule().may_repeat().args(2..));
        rules.add(RS_ID.rule().may_repeat().args(2..)); // may-repeat?
        rules
    };
*/
/// Rules for parsing a single routerstatus in a microdesc consensus
static NS_ROUTERSTATUS_RULES_MDCON: Lazy<SectionRules<NetstatusKwd>> = Lazy::new(|| {
    use NetstatusKwd::*;
    let mut rules = NS_ROUTERSTATUS_RULES_COMMON_.clone();
    rules.add(RS_R.rule().required().args(6..));
    rules.add(RS_M.rule().required().args(1..));
    rules
});
/// Rules for parsing consensus fields from a footer.
static NS_FOOTER_RULES: Lazy<SectionRules<NetstatusKwd>> = Lazy::new(|| {
    use NetstatusKwd::*;
    let mut rules = SectionRules::new();
    rules.add(DIRECTORY_FOOTER.rule().required().no_args());
    // consensus only
    rules.add(BANDWIDTH_WEIGHTS.rule());
    rules.add(UNRECOGNIZED.rule().may_repeat().obj_optional());
    rules
});

impl ProtoStatus {
    /// Construct a ProtoStatus from two chosen keywords in a section.
    fn from_section(
        sec: &Section<'_, NetstatusKwd>,
        recommend_token: NetstatusKwd,
        required_token: NetstatusKwd,
    ) -> Result<ProtoStatus> {
        /// Helper: extract a Protocols entry from an item's arguments.
        fn parse(t: Option<&Item<'_, NetstatusKwd>>) -> Result<Protocols> {
            if let Some(item) = t {
                item.args_as_str()
                    .parse::<Protocols>()
                    .map_err(|e| Error::BadArgument(item.pos(), e.to_string()))
            } else {
                Ok(Protocols::new())
            }
        }

        let recommended = parse(sec.get(recommend_token))?;
        let required = parse(sec.get(required_token))?;
        Ok(ProtoStatus {
            recommended,
            required,
        })
    }
}

impl<T> std::str::FromStr for NetParams<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error,
{
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        /// Helper: parse a single K=V pair.
        fn parse_pair<U>(p: &str) -> Result<(String, U)>
        where
            U: std::str::FromStr,
            U::Err: std::error::Error,
        {
            let parts: Vec<_> = p.splitn(2, '=').collect();
            if parts.len() != 2 {
                return Err(Error::BadArgument(
                    Pos::at(p),
                    "Missing = in key=value list".to_string(),
                ));
            }
            let num = parts[1]
                .parse::<U>()
                .map_err(|e| Error::BadArgument(Pos::at(parts[1]), e.to_string()))?;
            Ok((parts[0].to_string(), num))
        }

        let params = s
            .split(' ')
            .filter(|p| !p.is_empty())
            .map(parse_pair)
            .collect::<Result<HashMap<_, _>>>()?;
        Ok(NetParams { params })
    }
}

impl CommonHeader {
    /// Extract the CommonHeader members from a single header section.
    fn from_section(sec: &Section<'_, NetstatusKwd>) -> Result<CommonHeader> {
        use NetstatusKwd::*;

        {
            // this unwrap is safe because if there is not at least one
            // token in the section, the section is unparseable.
            let first = sec.first_item().unwrap();
            if first.kwd() != NETWORK_STATUS_VERSION {
                return Err(Error::UnexpectedToken(first.kwd().to_str(), first.pos()));
            }
        }

        let ver_item = sec.required(NETWORK_STATUS_VERSION)?;

        let version: u32 = ver_item.parse_arg(0)?;
        if version != 3 {
            return Err(Error::BadDocumentVersion(version));
        }
        let flavor = ConsensusFlavor::from_opt_name(ver_item.arg(1))?;

        let valid_after = sec
            .required(VALID_AFTER)?
            .args_as_str()
            .parse::<Iso8601TimeSp>()?
            .into();
        let fresh_until = sec
            .required(FRESH_UNTIL)?
            .args_as_str()
            .parse::<Iso8601TimeSp>()?
            .into();
        let valid_until = sec
            .required(VALID_UNTIL)?
            .args_as_str()
            .parse::<Iso8601TimeSp>()?
            .into();
        let lifetime = Lifetime::new(valid_after, fresh_until, valid_until)?;

        let client_versions = sec
            .maybe(CLIENT_VERSIONS)
            .args_as_str()
            .unwrap_or("")
            .split(',')
            .map(str::to_string)
            .collect();
        let relay_versions = sec
            .maybe(SERVER_VERSIONS)
            .args_as_str()
            .unwrap_or("")
            .split(',')
            .map(str::to_string)
            .collect();

        let client_protos = ProtoStatus::from_section(
            sec,
            RECOMMENDED_CLIENT_PROTOCOLS,
            REQUIRED_CLIENT_PROTOCOLS,
        )?;
        let relay_protos =
            ProtoStatus::from_section(sec, RECOMMENDED_RELAY_PROTOCOLS, REQUIRED_RELAY_PROTOCOLS)?;

        let params = sec.maybe(PARAMS).args_as_str().unwrap_or("").parse()?;

        let voting_delay = if let Some(tok) = sec.get(VOTING_DELAY) {
            let n1 = tok.parse_arg(0)?;
            let n2 = tok.parse_arg(1)?;
            Some((n1, n2))
        } else {
            None
        };

        Ok(CommonHeader {
            flavor,
            lifetime,
            client_versions,
            relay_versions,
            client_protos,
            relay_protos,
            params,
            voting_delay,
        })
    }
}

impl SharedRandVal {
    /// Parse a current or previous shared rand value from a given
    /// SharedRandPreviousValue or SharedRandCurrentValue.
    fn from_item(item: &Item<'_, NetstatusKwd>) -> Result<Self> {
        match item.kwd() {
            NetstatusKwd::SHARED_RAND_PREVIOUS_VALUE | NetstatusKwd::SHARED_RAND_CURRENT_VALUE => {}
            _ => return Err(Error::Internal(item.pos())),
        }
        let n_reveals: u8 = item.parse_arg(0)?;
        let val: B64 = item.parse_arg(1)?;
        let value = val.into();
        Ok(SharedRandVal { n_reveals, value })
    }
}

impl ConsensusHeader {
    /// Parse the ConsensusHeader members from a provided section.
    fn from_section(sec: &Section<'_, NetstatusKwd>) -> Result<ConsensusHeader> {
        use NetstatusKwd::*;

        let status: &str = sec.required(VOTE_STATUS)?.arg(0).unwrap_or("");
        if status != "consensus" {
            return Err(Error::BadDocumentType);
        }

        // We're ignoring KNOWN_FLAGS in the consensus.

        let hdr = CommonHeader::from_section(sec)?;

        let consensus_method: u32 = sec.required(CONSENSUS_METHOD)?.parse_arg(0)?;

        let shared_rand_prev = sec
            .get(SHARED_RAND_PREVIOUS_VALUE)
            .map(|i| SharedRandVal::from_item(i))
            .transpose()?;

        let shared_rand_cur = sec
            .get(SHARED_RAND_CURRENT_VALUE)
            .map(|i| SharedRandVal::from_item(i))
            .transpose()?;

        Ok(ConsensusHeader {
            hdr,
            consensus_method,
            shared_rand_prev,
            shared_rand_cur,
        })
    }
}

impl DirSource {
    /// Parse a "dir-source" item
    fn from_item(item: &Item<'_, NetstatusKwd>) -> Result<Self> {
        if item.kwd() != NetstatusKwd::DIR_SOURCE {
            return Err(Error::Internal(item.pos()));
        }
        let nickname = item.required_arg(0)?.to_string();
        let identity = item.parse_arg::<Fingerprint>(1)?.into();
        let address = item.required_arg(2)?.to_string();
        let ip = item.parse_arg(3)?;
        let dir_port = item.parse_arg(4)?;
        let or_port = item.parse_arg(5)?;

        Ok(DirSource {
            nickname,
            identity,
            address,
            ip,
            dir_port,
            or_port,
        })
    }
}

impl ConsensusVoterInfo {
    /// Parse a single ConsensusVoterInfo from a voter info section.
    fn from_section(sec: &Section<'_, NetstatusKwd>) -> Result<ConsensusVoterInfo> {
        use NetstatusKwd::*;
        if sec.first_item().unwrap().kwd() != DIR_SOURCE {
            return Err(Error::Internal(sec.first_item().unwrap().pos()));
        }
        let dir_source = DirSource::from_item(sec.required(DIR_SOURCE)?)?;

        let contact = sec.required(CONTACT)?.args_as_str().to_string();

        let vote_digest = sec.required(VOTE_DIGEST)?.parse_arg::<B16>(0)?.into();

        Ok(ConsensusVoterInfo {
            dir_source,
            contact,
            vote_digest,
        })
    }
}

impl std::str::FromStr for RelayFlags {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "Authority" => RelayFlags::AUTHORITY,
            "BadExit" => RelayFlags::BAD_EXIT,
            "Exit" => RelayFlags::EXIT,
            "Fast" => RelayFlags::FAST,
            "Guard" => RelayFlags::GUARD,
            "HSDir" => RelayFlags::HSDIR,
            "NoEdConsensus" => RelayFlags::NO_ED_CONSENSUS,
            "Stable" => RelayFlags::STABLE,
            "StaleDesc" => RelayFlags::STALE_DESC,
            "Running" => RelayFlags::RUNNING,
            "Valid" => RelayFlags::VALID,
            "V2Dir" => RelayFlags::V2DIR,
            _ => RelayFlags::empty(),
        })
    }
}

impl RelayFlags {
    /// Parse a relay-flags entry from an "s" line.
    fn from_item(item: &Item<'_, NetstatusKwd>) -> Result<RelayFlags> {
        if item.kwd() != NetstatusKwd::RS_S {
            return Err(Error::Internal(item.pos()));
        }
        // These flags are implicit.
        let mut flags: RelayFlags = RelayFlags::RUNNING | RelayFlags::VALID;

        let mut prev: Option<&str> = None;
        for s in item.args() {
            if let Some(p) = prev {
                if p >= s {
                    // Arguments out of order.
                    return Err(Error::BadArgument(
                        item.pos(),
                        "Flags out of order".to_string(),
                    ));
                }
            }
            let fl = s.parse().unwrap();
            flags |= fl;
            prev = Some(s);
        }

        Ok(flags)
    }
}

impl Default for RelayWeight {
    fn default() -> RelayWeight {
        RelayWeight::Unmeasured(0)
    }
}

impl RelayWeight {
    /// Parse a routerweight from a "w" line.
    fn from_item(item: &Item<'_, NetstatusKwd>) -> Result<RelayWeight> {
        if item.kwd() != NetstatusKwd::RS_W {
            return Err(Error::Internal(item.pos()));
        }

        let params: NetParams<u32> = item.args_as_str().parse()?;

        let bw = params.params.get("Bandwidth");
        let unmeas = params.params.get("Unmeasured");

        let bw = match bw {
            None => return Ok(RelayWeight::Unmeasured(0)),
            Some(b) => *b,
        };

        match unmeas {
            None | Some(0) => Ok(RelayWeight::Measured(bw)),
            Some(1) => Ok(RelayWeight::Unmeasured(bw)),
            _ => Err(Error::BadArgument(
                item.pos(),
                "unmeasured value".to_string(),
            )),
        }
    }
}

impl Footer {
    /// Parse a directory footer from a footer section.
    fn from_section(sec: &Section<'_, NetstatusKwd>) -> Result<Footer> {
        use NetstatusKwd::*;
        sec.required(DIRECTORY_FOOTER)?;

        let weights = sec
            .maybe(BANDWIDTH_WEIGHTS)
            .args_as_str()
            .unwrap_or("")
            .parse()?;

        Ok(Footer { weights })
    }
}

/// Result of checking a single authority signature.
enum SigCheckResult {
    /// The signature checks out.  Great!
    Valid,
    /// The signature is invalid; no additional information could make it
    /// valid.
    Invalid,
    /// We can't check the signatuer because we don't have a
    /// certificate with the right signing key.
    MissingCert,
}

impl Signature {
    /// Parse a Signature from a directory-signature section
    fn from_item(item: &Item<'_, NetstatusKwd>) -> Result<Signature> {
        if item.kwd() != NetstatusKwd::DIRECTORY_SIGNATURE {
            return Err(Error::Internal(item.pos()));
        }

        let (alg, id_fp, sk_fp) = if item.n_args() > 2 {
            (
                item.required_arg(0)?,
                item.required_arg(1)?,
                item.required_arg(2)?,
            )
        } else {
            ("sha1", item.required_arg(0)?, item.required_arg(1)?)
        };

        let digestname = alg.to_string();
        let id_fingerprint = id_fp.parse::<Fingerprint>()?.into();
        let sk_fingerprint = sk_fp.parse::<Fingerprint>()?.into();
        let key_ids = AuthCertKeyIds {
            id_fingerprint,
            sk_fingerprint,
        };
        let signature = item.obj("SIGNATURE")?;

        Ok(Signature {
            digestname,
            key_ids,
            signature,
        })
    }

    /// Return true if this signature has the identity key and signing key
    /// that match a given cert.
    fn matches_cert(&self, cert: &AuthCert) -> bool {
        cert.key_ids() == &self.key_ids
    }

    /// If possible, find the right certificate for checking this signature
    /// from among a slice of certificates.
    fn find_cert<'a>(&self, certs: &'a [AuthCert]) -> Option<&'a AuthCert> {
        for c in certs {
            if self.matches_cert(c) {
                return Some(c);
            }
        }
        None
    }

    /// Try to check whether this signature is a valid signature of a
    /// provided digest, given a slice of certificates that might contain
    /// its signing key.
    fn check_signature(&self, signed_digest: &[u8], certs: &[AuthCert]) -> SigCheckResult {
        match self.find_cert(certs) {
            None => SigCheckResult::MissingCert,
            Some(cert) => {
                let key = cert.signing_key();
                match key.verify(signed_digest, &self.signature[..]) {
                    Ok(()) => SigCheckResult::Valid,
                    Err(_) => SigCheckResult::Invalid,
                }
            }
        }
    }
}

/// A Consensus object that has been parsed, but not checked for
/// signatures and timeliness.
pub type UncheckedConsensus<RS> = TimerangeBound<UnvalidatedConsensus<RS>>;

impl<RS: RouterStatus + ParseRouterStatus> Consensus<RS> {
    /// Return a new ConsensusBuilder for building test consensus objects.
    ///
    /// This function is only available when the `build_docs` feature has
    /// been enabled.
    #[cfg(feature = "build_docs")]
    pub fn builder() -> ConsensusBuilder<RS> {
        ConsensusBuilder::new(RS::flavor())
    }

    /// Try to parse a single networkstatus document from a string.
    pub fn parse(s: &str) -> Result<(&str, &str, UncheckedConsensus<RS>)> {
        let mut reader = NetDocReader::new(s);
        Self::parse_from_reader(&mut reader).map_err(|e| e.within(s))
    }
    /// Extract a voter-info section from the reader; return
    /// Ok(None) when we are out of voter-info sections.
    fn take_voterinfo(
        r: &mut NetDocReader<'_, NetstatusKwd>,
    ) -> Result<Option<ConsensusVoterInfo>> {
        use NetstatusKwd::*;

        match r.iter().peek() {
            None => return Ok(None),
            Some(e) if e.is_ok_with_kwd_in(&[RS_R, DIRECTORY_FOOTER]) => return Ok(None),
            _ => (),
        };

        let mut first_dir_source = true;
        // TODO: Extract this pattern into a "pause at second"???
        // Pause at the first 'r', or the second 'dir-source'.
        let mut p = r.pause_at(|i| match i {
            Err(_) => false,
            Ok(item) => {
                item.kwd() == RS_R
                    || if item.kwd() == DIR_SOURCE {
                        let was_first = first_dir_source;
                        first_dir_source = false;
                        !was_first
                    } else {
                        false
                    }
            }
        });

        let voter_sec = NS_VOTERINFO_RULES_CONSENSUS.parse(&mut p)?;
        let voter = ConsensusVoterInfo::from_section(&voter_sec)?;

        Ok(Some(voter))
    }

    /// Extract the footer (but not signatures) from the reader.
    fn take_footer(r: &mut NetDocReader<'_, NetstatusKwd>) -> Result<Footer> {
        use NetstatusKwd::*;
        let mut p = r.pause_at(|i| i.is_ok_with_kwd_in(&[DIRECTORY_SIGNATURE]));
        let footer_sec = NS_FOOTER_RULES.parse(&mut p)?;
        let footer = Footer::from_section(&footer_sec)?;
        Ok(footer)
    }

    /// Extract a routerstatus from the reader.  Return Ok(None) if we're
    /// out of routerstatus entries.
    fn take_routerstatus(r: &mut NetDocReader<'_, NetstatusKwd>) -> Result<Option<(Pos, RS)>> {
        use NetstatusKwd::*;
        match r.iter().peek() {
            None => return Ok(None),
            Some(e) if e.is_ok_with_kwd_in(&[DIRECTORY_FOOTER]) => return Ok(None),
            _ => (),
        };

        let pos = r.pos();

        let mut first_r = true;
        let mut p = r.pause_at(|i| match i {
            Err(_) => false,
            Ok(item) => {
                item.kwd() == DIRECTORY_FOOTER
                    || if item.kwd() == RS_R {
                        let was_first = first_r;
                        first_r = false;
                        !was_first
                    } else {
                        false
                    }
            }
        });

        let rules = match RS::flavor() {
            ConsensusFlavor::Microdesc => &NS_ROUTERSTATUS_RULES_MDCON,
            ConsensusFlavor::Ns => &NS_ROUTERSTATUS_RULES_NSCON,
        };

        let rs_sec = rules.parse(&mut p)?;
        let rs = RS::from_section(&rs_sec)?;
        Ok(Some((pos, rs)))
    }

    /// Extract an entire UncheckedConsensus from a reader.
    ///
    /// Returns the signed portion of the string, the remainder of the
    /// string, and an UncheckedConsensus.
    fn parse_from_reader<'a>(
        r: &mut NetDocReader<'a, NetstatusKwd>,
    ) -> Result<(&'a str, &'a str, UncheckedConsensus<RS>)> {
        use NetstatusKwd::*;
        let (header, start_pos) = {
            let mut h = r.pause_at(|i| i.is_ok_with_kwd_in(&[DIR_SOURCE]));
            let header_sec = NS_HEADER_RULES_CONSENSUS.parse(&mut h)?;
            let pos = header_sec.first_item().unwrap().offset_in(r.str());
            (ConsensusHeader::from_section(&header_sec)?, pos.unwrap())
        };
        if RS::flavor() != header.hdr.flavor {
            return Err(Error::BadDocumentType);
        }

        let mut voters = Vec::new();

        while let Some(voter) = Self::take_voterinfo(r)? {
            voters.push(voter);
        }

        let mut relays: Vec<RS> = Vec::new();
        while let Some((pos, routerstatus)) = Self::take_routerstatus(r)? {
            if let Some(prev) = relays.last() {
                if prev.rsa_identity() >= routerstatus.rsa_identity() {
                    return Err(Error::WrongSortOrder(pos));
                }
            }
            relays.push(routerstatus);
        }

        let footer = Self::take_footer(r)?;

        let consensus = Consensus {
            header,
            voters,
            relays,
            footer,
        };

        // Find the signatures.
        let mut first_sig: Option<Item<'_, NetstatusKwd>> = None;
        let mut signatures = Vec::new();
        for item in r.iter() {
            let item = item?;
            if item.kwd() != DIRECTORY_SIGNATURE {
                return Err(Error::UnexpectedToken(item.kwd().to_str(), item.pos()));
            }

            let sig = Signature::from_item(&item)?;
            if first_sig.is_none() {
                first_sig = Some(item);
            }
            signatures.push(sig);
        }

        if first_sig.is_none() {
            return Err(Error::MissingToken("directory-signature"));
        }

        let end_pos = first_sig.unwrap().offset_in(r.str()).unwrap() + "directory-signature ".len();

        // Find the appropriate digest.
        let signed_str = &r.str()[start_pos..end_pos];
        let remainder = &r.str()[end_pos..];
        let (sha256, sha1) = match RS::flavor() {
            ConsensusFlavor::Ns => (
                None,
                Some(ll::d::Sha1::digest(signed_str.as_bytes()).into()),
            ),
            ConsensusFlavor::Microdesc => (
                Some(ll::d::Sha256::digest(signed_str.as_bytes()).into()),
                None,
            ),
        };
        let siggroup = SignatureGroup {
            sha256,
            sha1,
            signatures,
        };

        let unval = UnvalidatedConsensus {
            consensus,
            siggroup,
            n_authorities: None,
        };
        let lifetime = unval.consensus.header.hdr.lifetime.clone();
        let delay = unval.consensus.header.hdr.voting_delay.unwrap_or((0, 0));
        let dist_interval = time::Duration::from_secs(delay.1.into());
        let starting_time = lifetime.valid_after - dist_interval;
        let timebound = TimerangeBound::new(unval, starting_time..lifetime.valid_until);
        Ok((signed_str, remainder, timebound))
    }
}

/// A Microdesc consensus whose signatures have not yet been checked.
///
/// To validate this object, call set_n_authorities() on it, then call
/// check_signature() on that result with the set of certs that you
/// have.  Make sure only to provide authority certificates representing
/// real authorities!
#[derive(Debug, Clone)]
pub struct UnvalidatedConsensus<RS> {
    /// The consensus object. We don't want to expose this until it's
    /// validated.
    consensus: Consensus<RS>,
    /// The signatures that need to be validated before we can call
    /// this consensus valid.
    siggroup: SignatureGroup,
    /// The total number of authorities that we believe in.  We need
    /// this information in order to validate the signatures, since it
    /// determines how many signatures we need to find valid in `siggroup`.
    n_authorities: Option<u16>,
}

impl<RS> UnvalidatedConsensus<RS> {
    /// Tell the unvalidated consensus how many authorities we believe in.
    ///
    /// Without knowing this number, we can't validate the signature.
    pub fn set_n_authorities(self, n_authorities: u16) -> Self {
        UnvalidatedConsensus {
            n_authorities: Some(n_authorities),
            ..self
        }
    }

    /// Return an iterator of all the certificate IDs that we might use
    /// to validate this consensus.
    pub fn signing_cert_ids(&self) -> impl Iterator<Item = AuthCertKeyIds> {
        match self.key_is_correct(&[]) {
            Ok(()) => Vec::new(),
            Err(missing) => missing,
        }
        .into_iter()
    }

    /// Return the lifetime of this unvalidated consensus
    pub fn peek_lifetime(&self) -> &Lifetime {
        self.consensus.lifetime()
    }

    /// Return true if a client who believes in exactly the provided
    /// set of authority IDs might might consider this consensus to be
    /// well-signed.
    ///
    /// (This is the case if the consensus claims to be signed by more than
    /// half of the authorities in the list.)
    pub fn authorities_are_correct(&self, authorities: &[&RsaIdentity]) -> bool {
        self.siggroup.could_validate(authorities)
    }
}

impl<RS> ExternallySigned<Consensus<RS>> for UnvalidatedConsensus<RS> {
    type Key = [AuthCert];
    type KeyHint = Vec<AuthCertKeyIds>;
    type Error = Error;

    fn key_is_correct(&self, k: &Self::Key) -> result::Result<(), Self::KeyHint> {
        let (n_ok, missing) = self.siggroup.list_missing(k);
        match self.n_authorities {
            Some(n) if n_ok > (n / 2) as usize => Ok(()),
            _ => Err(missing.iter().map(|cert| cert.key_ids).collect()),
        }
    }
    fn is_well_signed(&self, k: &Self::Key) -> result::Result<(), Self::Error> {
        if self.n_authorities.is_none() {
            return Err(Error::Internal(Pos::None));
        }
        if self.siggroup.validate(self.n_authorities.unwrap(), k) {
            Ok(())
        } else {
            Err(Error::BadSignature(Pos::None))
        }
    }
    fn dangerously_assume_wellsigned(self) -> Consensus<RS> {
        self.consensus
    }
}

impl SignatureGroup {
    // TODO: these functions are pretty similar and could probably stand to be
    // refactored a lot.

    /// Helper: Return a pair of the number of possible authorities'
    /// signatures in this object for which we _could_ find certs, and
    /// a list of the signatures we couldn't find certificates for.
    fn list_missing(&self, certs: &[AuthCert]) -> (usize, Vec<&Signature>) {
        let mut ok: HashSet<RsaIdentity> = HashSet::new();
        let mut missing = Vec::new();
        for sig in self.signatures.iter() {
            let id_fingerprint = &sig.key_ids.id_fingerprint;
            if ok.contains(id_fingerprint) {
                continue;
            }
            if sig.find_cert(certs).is_some() {
                ok.insert(*id_fingerprint);
                continue;
            }

            missing.push(sig);
        }
        (ok.len(), missing)
    }

    /// Given a list of authority identity key fingerprints, return true if
    /// this signature group is _potentially_ well-signed according to those
    /// authorities.
    fn could_validate(&self, authorities: &[&RsaIdentity]) -> bool {
        let mut signed_by: HashSet<RsaIdentity> = HashSet::new();
        for sig in self.signatures.iter() {
            let id_fp = &sig.key_ids.id_fingerprint;
            if signed_by.contains(id_fp) {
                // Already found this in the list.
                continue;
            }
            if authorities.contains(&id_fp) {
                signed_by.insert(*id_fp);
            }
        }

        signed_by.len() > (authorities.len() / 2)
    }

    /// Return true if the signature group defines a valid signature.
    ///
    /// A signature is valid if it signed by more than half of the
    /// authorities.  This API requires that `n_authorities` is the number of
    /// authorities we believe in, and that every cert in `certs` belongs
    /// to a real authority.
    fn validate(&self, n_authorities: u16, certs: &[AuthCert]) -> bool {
        // A set of the authorities (by identity) who have have signed
        // this document.  We use a set here in case `certs` has more
        // than one certificate for a single authority.
        let mut ok: HashSet<RsaIdentity> = HashSet::new();

        for sig in self.signatures.iter() {
            let id_fingerprint = &sig.key_ids.id_fingerprint;
            if ok.contains(id_fingerprint) {
                // We already checked at least one signature using this
                // authority's identity fingerprint.
                continue;
            }

            let d: Option<&[u8]> = match sig.digestname.as_ref() {
                "sha256" => self.sha256.as_ref().map(|a| &a[..]),
                "sha1" => self.sha1.as_ref().map(|a| &a[..]),
                _ => None, // We don't know how to find this digest.
            };
            if d.is_none() {
                // We don't support this kind of digest for this kind
                // of document.
                continue;
            }

            match sig.check_signature(d.as_ref().unwrap(), certs) {
                SigCheckResult::Valid => {
                    ok.insert(*id_fingerprint);
                }
                _ => continue,
            }
        }

        ok.len() > (n_authorities / 2) as usize
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use hex_literal::hex;

    const CERTS: &str = include_str!("../../testdata/authcerts2.txt");
    const CONSENSUS: &str = include_str!("../../testdata/mdconsensus1.txt");

    const NS_CERTS: &str = include_str!("../../testdata/authcerts3.txt");
    const NS_CONSENSUS: &str = include_str!("../../testdata/nsconsensus1.txt");

    fn read_bad(fname: &str) -> String {
        use std::fs;
        use std::path::PathBuf;
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("testdata");
        path.push("bad-mdconsensus");
        path.push(fname);

        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn parse_and_validate_md() -> Result<()> {
        use std::net::SocketAddr;
        use tor_checkable::{SelfSigned, Timebound};
        let mut certs = Vec::new();
        for cert in AuthCert::parse_multiple(CERTS) {
            let cert = cert?.check_signature()?.dangerously_assume_timely();
            certs.push(cert);
        }
        let auth_ids: Vec<_> = certs.iter().map(|c| &c.key_ids().id_fingerprint).collect();

        assert_eq!(certs.len(), 3);

        let (_, _, consensus) = MdConsensus::parse(CONSENSUS)?;
        let consensus = consensus.dangerously_assume_timely().set_n_authorities(3);

        // The set of authorities we know _could_ validate this cert.
        assert!(consensus.authorities_are_correct(&auth_ids));
        // A subset would also work.
        assert!(consensus.authorities_are_correct(&auth_ids[0..1]));
        {
            // If we only believe in an authority that isn't listed,
            // that won't work.
            let bad_auth_id = (*b"xxxxxxxxxxxxxxxxxxxx").into();
            assert!(!consensus.authorities_are_correct(&[&bad_auth_id]));
        }

        let missing = consensus.key_is_correct(&[]).err().unwrap();
        assert_eq!(3, missing.len());
        assert!(consensus.key_is_correct(&certs).is_ok());
        let missing = consensus.key_is_correct(&certs[0..1]).err().unwrap();
        assert_eq!(2, missing.len());

        // here is a trick that had better not work.
        let mut same_three_times = Vec::new();
        same_three_times.push(certs[0].clone());
        same_three_times.push(certs[0].clone());
        same_three_times.push(certs[0].clone());
        let missing = consensus.key_is_correct(&same_three_times).err().unwrap();

        assert_eq!(2, missing.len());
        assert!(consensus.is_well_signed(&same_three_times).is_err());

        assert!(consensus.key_is_correct(&certs).is_ok());
        let consensus = consensus.check_signature(&certs)?;

        assert_eq!(6, consensus.relays().len());
        let r0 = &consensus.relays()[0];
        assert_eq!(
            r0.md_digest(),
            &hex!("73dabe0a0468f4f7a67810a18d11e36731bb1d2ec3634db459100609f3b3f535")
        );
        assert_eq!(
            r0.rsa_identity().as_bytes(),
            &hex!("0a3057af2910415794d8ea430309d9ac5f5d524b")
        );
        assert_eq!(r0.weight().is_measured(), false);
        assert_eq!(r0.weight().is_nonzero(), false);
        let pv = &r0.protovers();
        assert!(pv.supports_subver("HSDir", 2));
        assert!(!pv.supports_subver("HSDir", 3));
        let ip4 = "127.0.0.1:5002".parse::<SocketAddr>().unwrap();
        let ip6 = "[::1]:5002".parse::<SocketAddr>().unwrap();
        assert!(r0.orport_addrs().any(|a| a == &ip4));
        assert!(r0.orport_addrs().any(|a| a == &ip6));

        Ok(())
    }

    #[test]
    fn parse_and_validate_ns() -> Result<()> {
        use tor_checkable::{SelfSigned, Timebound};
        let mut certs = Vec::new();
        for cert in AuthCert::parse_multiple(NS_CERTS) {
            let cert = cert?.check_signature()?.dangerously_assume_timely();
            certs.push(cert);
        }
        let auth_ids: Vec<_> = certs.iter().map(|c| &c.key_ids().id_fingerprint).collect();
        assert_eq!(certs.len(), 3);

        let (_, _, consensus) = NsConsensus::parse(NS_CONSENSUS)?;
        let consensus = consensus.dangerously_assume_timely().set_n_authorities(3);
        // The set of authorities we know _could_ validate this cert.
        assert!(consensus.authorities_are_correct(&auth_ids));
        // A subset would also work.
        assert!(consensus.authorities_are_correct(&auth_ids[0..1]));

        assert!(consensus.key_is_correct(&certs).is_ok());
        dbg!("FOO");
        let _consensus = consensus.check_signature(&certs)?;

        Ok(())
    }

    #[test]
    fn test_bad() {
        use crate::Pos;
        fn check(fname: &str, e: Error) {
            let content = read_bad(fname);
            let res = MdConsensus::parse(&content);
            assert!(res.is_err());
            assert_eq!(res.err().unwrap(), e);
        }

        check(
            "bad-flags",
            Error::BadArgument(Pos::from_line(27, 1), "Flags out of order".into()),
        );
        check(
            "bad-md-digest",
            Error::BadArgument(Pos::from_line(40, 3), "Invalid base64".into()),
        );
        check(
            "bad-weight",
            Error::BadArgument(
                Pos::from_line(67, 141),
                "invalid digit found in string".into(),
            ),
        );
        check(
            "bad-weights",
            Error::BadArgument(
                Pos::from_line(51, 13),
                "invalid digit found in string".into(),
            ),
        );
        check("wrong-order", Error::WrongSortOrder(Pos::from_line(52, 1)));
        check(
            "wrong-start",
            Error::UnexpectedToken("vote-status".into(), Pos::from_line(1, 1)),
        );
        check("wrong-version", Error::BadDocumentVersion(10));
    }

    fn gettok(s: &str) -> Result<Item<'_, NetstatusKwd>> {
        let mut reader = NetDocReader::new(s);
        let it = reader.iter();
        let tok = it.next().unwrap();
        assert!(it.next().is_none());
        tok
    }

    #[test]
    fn test_weight() {
        let w = gettok("w Unmeasured=1 Bandwidth=6\n").unwrap();
        let w = RelayWeight::from_item(&w).unwrap();
        assert!(!w.is_measured());
        assert!(w.is_nonzero());

        let w = gettok("w Bandwidth=10\n").unwrap();
        let w = RelayWeight::from_item(&w).unwrap();
        assert!(w.is_measured());
        assert!(w.is_nonzero());

        let w = RelayWeight::default();
        assert!(!w.is_measured());
        assert!(!w.is_nonzero());

        let w = gettok("w Mustelid=66 Cheato=7 Unmeasured=1\n").unwrap();
        let w = RelayWeight::from_item(&w).unwrap();
        assert!(!w.is_measured());
        assert!(!w.is_nonzero());

        let w = gettok("r foo\n").unwrap();
        let w = RelayWeight::from_item(&w);
        assert!(w.is_err());

        let w = gettok("r Bandwidth=6 Unmeasured=Frog\n").unwrap();
        let w = RelayWeight::from_item(&w);
        assert!(w.is_err());

        let w = gettok("r Bandwidth=6 Unmeasured=3\n").unwrap();
        let w = RelayWeight::from_item(&w);
        assert!(w.is_err());
    }

    #[test]
    fn test_netparam() {
        let p = "Hello=600 Goodbye=5 Fred=7"
            .parse::<NetParams<u32>>()
            .unwrap();
        assert_eq!(p.get("Hello"), Some(&600_u32));

        let p = "Hello=Goodbye=5 Fred=7".parse::<NetParams<u32>>();
        assert!(p.is_err());

        let p = "Hello=Goodbye Fred=7".parse::<NetParams<u32>>();
        assert!(p.is_err());
    }

    #[test]
    fn test_sharedrand() {
        let sr =
            gettok("shared-rand-previous-value 9 5LodY4yWxFhTKtxpV9wAgNA9N8flhUCH0NqQv1/05y4\n")
                .unwrap();
        let sr = SharedRandVal::from_item(&sr).unwrap();

        assert_eq!(sr.n_reveals, 9);
        assert_eq!(
            sr.value,
            hex!("e4ba1d638c96c458532adc6957dc0080d03d37c7e5854087d0da90bf5ff4e72e")
        );

        let sr = gettok("foo bar\n").unwrap();
        let sr = SharedRandVal::from_item(&sr);
        assert!(sr.is_err());
    }
}
