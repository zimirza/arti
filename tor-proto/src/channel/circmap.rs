// NOTE: This is a work in progress and I bet I'll refactor it a lot;
// it needs to stay opaque!

use crate::chancell::msg::ChanMsg;
use crate::chancell::CircID;
use crate::util::idmap::IdMap;
use crate::Result;

use futures::channel::mpsc;

use rand::Rng;

/// Which group of circuit IDs are we allowed to allocate in this map?
///
/// If we initiated the channel, we use High circuit ids.  If we're the
/// responder, we use low circuit ids.
pub(super) enum CircIDRange {
    Low,
    High,
}

impl rand::distributions::Distribution<CircID> for CircIDRange {
    /// Return a random circuit ID in the appropriate range.
    fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> CircID {
        // Make sure v is nonzero.
        let v = loop {
            match rng.gen() {
                0u32 => (),
                x => break x,
            }
        };
        // Force the high bit of v to the appropriate value.
        match self {
            CircIDRange::Low => v & 0x7fff_ffff,
            CircIDRange::High => v | 0x8000_0000,
        }
        .into()
    }
}

/// An entry in the circuit map.  Right now, we only have "here's the
/// way to send cells to a given circuit", but that's likely to
/// change.
pub(super) enum CircEnt {
    Open(mpsc::Sender<ChanMsg>),
}

/// A map from circuit IDs to circuit entries. Each channel has one.
pub(super) struct CircMap {
    m: IdMap<CircID, CircIDRange, CircEnt>,
}

impl CircMap {
    /// Make a new empty CircMap
    pub(super) fn new(idrange: CircIDRange) -> Self {
        CircMap {
            m: IdMap::new(idrange),
        }
    }

    pub(super) fn add_ent<R: Rng>(
        &mut self,
        rng: &mut R,
        sink: mpsc::Sender<ChanMsg>,
    ) -> Result<CircID> {
        let ent = CircEnt::Open(sink);
        self.m.add_ent(rng, ent)
    }

    /// Return the entry for `id` in this map, if any.
    pub(super) fn get_mut(&mut self, id: CircID) -> Option<&mut CircEnt> {
        self.m.get_mut(&id)
    }

    // TODO: Eventually if we want relay support, we'll need to support
    // circuit IDs chosen by somebody else. But for now, we don't need those.
}
