//! Code to handle incoming cells on a circuit.
use super::streammap::{ShouldSendEnd, StreamEnt};
use crate::circuit::celltypes::{ClientCircChanMsg, CreateResponse};
use crate::circuit::unique_id::UniqId;
use crate::circuit::{
    sendme, streammap, CircParameters, Create2Wrap, CreateFastWrap, CreateHandshakeWrap,
};
use crate::crypto::cell::{
    ClientLayer, CryptInit, HopNum, InboundClientCrypt, InboundClientLayer, OutboundClientCrypt,
    OutboundClientLayer, RelayCellBody, Tor1RelayCrypto,
};
use crate::util::err::ReactorError;
use crate::{Error, Result};
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::pin::Pin;
use tor_cell::chancell::msg::{ChanMsg, Relay};
use tor_cell::relaycell::msg::{End, RelayMsg, Sendme};
use tor_cell::relaycell::{RelayCell, RelayCmd, StreamId};

use futures::channel::{mpsc, oneshot};
use futures::Sink;
use futures::Stream;

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::channel::Channel;
#[cfg(test)]
use crate::circuit::sendme::CircTag;
use crate::circuit::sendme::StreamSendWindow;
use crate::crypto::handshake::ntor::{NtorClient, NtorPublicKey};
use crate::crypto::handshake::{ClientHandshake, KeyGenerator};
use tor_cell::chancell;
use tor_cell::chancell::{ChanCell, CircId};
use tor_linkspec::LinkSpec;
use tor_llcrypto::pk;
use tracing::{debug, trace, warn};

/// Initial value for outbound flow-control window on streams.
pub(super) const SEND_WINDOW_INIT: u16 = 500;
/// Initial value for inbound flow-control window on streams.
pub(super) const RECV_WINDOW_INIT: u16 = 500;
/// Size of the buffer used between the reactor and a `StreamReader`.
///
/// FIXME(eta): We pick 2× the receive window, which is very conservative (we arguably shouldn't
///             get sent more than the receive window anyway!). We might do due to things that
///             don't count towards the window though.
pub(super) const STREAM_READER_BUFFER: usize = (2 * RECV_WINDOW_INIT) as usize;

/// The type of a oneshot channel used to inform reactor users of the result of an operation.
pub(super) type ReactorResultChannel<T> = oneshot::Sender<Result<T>>;

/// A handshake type, to be used when creating circuit hops.
#[derive(Clone, Debug)]
pub(super) enum CircuitHandshake {
    /// Use the CREATE_FAST handshake.
    CreateFast,
    /// Use the ntor handshake.
    Ntor {
        /// The public key of the relay.
        public_key: NtorPublicKey,
        /// The first hop's Ed25519 identity, which is verified against
        /// the identity held in the circuit's channel.
        ed_identity: pk::ed25519::Ed25519Identity,
    },
}

/// A message telling the reactor to do something.
#[derive(Debug)]
pub(super) enum CtrlMsg {
    /// Create the first hop of this circuit.
    Create {
        /// A oneshot channel on which we'll receive the creation response.
        recv_created: oneshot::Receiver<CreateResponse>,
        /// The handshake type to use for the first hop.
        handshake: CircuitHandshake,
        /// Whether the hop supports authenticated SENDME cells.
        supports_authenticated_sendme: bool,
        /// Other parameters relevant for circuit creation.
        params: CircParameters,
        /// Oneshot channel to notify on completion.
        done: ReactorResultChannel<()>,
    },
    /// Extend a circuit by one hop, using the ntor handshake.
    ExtendNtor {
        /// The handshake type to use for this hop.
        public_key: NtorPublicKey,
        /// Information about how to connect to the relay we're extending to.
        linkspecs: Vec<LinkSpec>,
        /// Whether the hop supports authenticated SENDME cells.
        supports_authenticated_sendme: bool,
        /// Other parameters relevant for circuit extension.
        params: CircParameters,
        /// Oneshot channel to notify on completion.
        done: ReactorResultChannel<()>,
    },
    /// Begin a stream with the provided hop in this circuit.
    ///
    /// Allocates a stream ID, and sends the provided message to that hop.
    BeginStream {
        /// The hop number to begin the stream with.
        hop_num: HopNum,
        /// The message to send.
        message: RelayMsg,
        /// A channel to send messages on this stream down.
        ///
        /// This sender shouldn't ever block, because we use congestion control and only send
        /// SENDME cells once we've read enough out of the other end. If it *does* block, we
        /// can assume someone is trying to send us more cells than they should, and abort
        /// the stream.
        sender: mpsc::Sender<RelayMsg>,
        /// A channel to receive messages to send on this stream from.
        rx: mpsc::Receiver<RelayMsg>,
        /// Oneshot channel to notify on completion, with the allocated stream ID.
        done: ReactorResultChannel<StreamId>,
    },
    /// Send a SENDME cell (used to ask for more data to be sent) on the given stream.
    SendSendme {
        /// The stream ID to send a SENDME for.
        stream_id: StreamId,
        /// The hop number the stream is on.
        hop_num: HopNum,
    },
    /// Shut down the reactor.
    Shutdown,
    /// (tests only) Add a hop to the list of hops on this circuit, with dummy cryptography.
    #[cfg(test)]
    AddFakeHop {
        supports_flowctrl_1: bool,
        fwd_lasthop: bool,
        rev_lasthop: bool,
        params: CircParameters,
        done: ReactorResultChannel<()>,
    },
    /// (tests only) Get the send window and expected tags for a given hop.
    #[cfg(test)]
    QuerySendWindow {
        hop: HopNum,
        done: ReactorResultChannel<(u16, Vec<CircTag>)>,
    },
    /// (tests only) Send a raw relay cell with send_relay_cell().
    #[cfg(test)]
    SendRelayCell {
        hop: HopNum,
        early: bool,
        cell: RelayCell,
    },
}
/// Represents the reactor's view of a single hop.
pub(super) struct CircHop {
    /// Map from stream IDs to streams.
    ///
    /// We store this with the reactor instead of the circuit, since the
    /// reactor needs it for every incoming cell on a stream, whereas
    /// the circuit only needs it when allocating new streams.
    map: streammap::StreamMap,
    /// Window used to say how many cells we can receive.
    recvwindow: sendme::CircRecvWindow,
    /// If true, this hop is using an older link protocol and we
    /// shouldn't expect good authenticated SENDMEs from it.
    auth_sendme_optional: bool,
    /// Window used to say how many cells we can send.
    sendwindow: sendme::CircSendWindow,
    /// Buffer for messages we can't send to this hop yet due to congestion control.
    ///
    /// Contains the tag we should give to the send window, and the cell to send.
    ///
    /// This shouldn't grow unboundedly: we try and pop things off it first before
    /// doing things that would result in it growing (and stop before growing it
    /// if popping things off it can't be done).
    outbound: VecDeque<([u8; 20], ChanCell)>,
}

impl CircHop {
    /// Create a new hop.
    pub(super) fn new(auth_sendme_optional: bool, initial_window: u16) -> Self {
        CircHop {
            map: streammap::StreamMap::new(),
            recvwindow: sendme::CircRecvWindow::new(1000),
            auth_sendme_optional,
            sendwindow: sendme::CircSendWindow::new(initial_window),
            outbound: VecDeque::new(),
        }
    }
}

/// An object that's waiting for a meta cell (one not associated with a stream) in order to make
/// progress.
///
/// # Background
///
/// The `Reactor` can't have async functions that send and receive cells, because its job is to
/// send and receive cells: if one of its functions tried to do that, it would just hang forever.
///
/// To get around this problem, the reactor can send some cells, and then make one of these
/// `MetaCellHandler` objects, which will be run when the reply arrives.
pub(super) trait MetaCellHandler: Send {
    /// The hop we're expecting the message to come from. This is compared against the hop
    /// from which we actually receive messages, and an error is thrown if the two don't match.
    fn expected_hop(&self) -> HopNum;
    /// Called when the message we were waiting for arrives.
    ///
    /// Gets a copy of the `Reactor` in order to do anything it likes there.
    fn finish(&mut self, msg: RelayMsg, reactor: &mut Reactor) -> Result<()>;
}

/// An object that can extend a circuit by one hop, using the `MetaCellHandler` trait.
///
/// Yes, I know having trait bounds on structs is bad, but in this case it's necessary
/// since we want to be able to use `H::KeyType`.
struct CircuitExtender<H, L, FWD, REV>
where
    H: ClientHandshake,
{
    /// Handshake state.
    state: Option<H::StateType>,
    /// Whether the hop supports authenticated SENDME cells.
    supports_flowctrl_1: bool,
    /// Parameters used for this extension.
    params: CircParameters,
    /// An identifier for logging about this reactor's circuit.
    unique_id: UniqId,
    /// The hop we're expecting the EXTENDED2 cell to come back from.
    expected_hop: HopNum,
    /// `PhantomData` used to make the other type parameters required for a circuit extension
    /// part of the `struct`, instead of having them be provided during a function call.
    ///
    /// This is done this way so we can implement `MetaCellHandler` for this type, which
    /// doesn't include any generic type parameters; we need them to be part of the type
    /// so we know what they are for that `impl` block.
    phantom: PhantomData<(L, FWD, REV)>,
}
impl<H, L, FWD, REV> CircuitExtender<H, L, FWD, REV>
where
    H: ClientHandshake,
    H::KeyGen: KeyGenerator,
    L: CryptInit + ClientLayer<FWD, REV>,
    FWD: OutboundClientLayer + 'static + Send,
    REV: InboundClientLayer + 'static + Send,
{
    /// Start extending a circuit, sending the necessary EXTEND cell and returning a
    /// new `CircuitExtender` to be called when the reply arrives.
    ///
    /// The `handshake_id` is the numeric identifier for what kind of
    /// handshake we're doing.  The `key` is the relay's onion key that
    /// goes along with the handshake, and the `linkspecs` are the
    /// link specifiers to include in the EXTEND cell to tell the
    /// current last hop which relay to connect to.
    fn begin(
        cx: &mut Context<'_>,
        handshake_id: u16,
        key: &H::KeyType,
        linkspecs: Vec<LinkSpec>,
        supports_flowctrl_1: bool,
        params: CircParameters,
        reactor: &mut Reactor,
    ) -> Result<Self> {
        let mut rng = rand::thread_rng();
        let unique_id = reactor.unique_id;

        use tor_cell::relaycell::msg::{Body, Extend2};
        // Perform the first part of the cryptographic handshake
        let (state, msg) = H::client1(&mut rng, key)?;

        let n_hops = reactor.crypto_out.n_layers();
        let hop = ((n_hops - 1) as u8).into();

        debug!(
            "{}: Extending circuit to hop {} with {:?}",
            unique_id,
            n_hops + 1,
            linkspecs
        );

        let extend_msg = Extend2::new(linkspecs, handshake_id, msg);
        let cell = RelayCell::new(0.into(), extend_msg.into_message());

        // Send the message to the last hop...
        reactor.send_relay_cell(
            cx, hop, true, // use a RELAY_EARLY cell
            cell,
        )?;
        trace!("{}: waiting for EXTENDED2 cell", unique_id);
        // ... and now we wait for a response.

        Ok(Self {
            state: Some(state),
            supports_flowctrl_1,
            params,
            unique_id,
            expected_hop: hop,
            phantom: Default::default(),
        })
    }
}

impl<H, L, FWD, REV> MetaCellHandler for CircuitExtender<H, L, FWD, REV>
where
    H: ClientHandshake,
    H::StateType: Send,
    H::KeyGen: KeyGenerator,
    L: CryptInit + ClientLayer<FWD, REV> + Send,
    FWD: OutboundClientLayer + 'static + Send,
    REV: InboundClientLayer + 'static + Send,
{
    fn expected_hop(&self) -> HopNum {
        self.expected_hop
    }
    fn finish(&mut self, msg: RelayMsg, reactor: &mut Reactor) -> Result<()> {
        // XXXX If two EXTEND cells are of these are launched on the
        // same circuit at once, could they collide in this part of
        // the function?  I don't _think_ so, but it might be a good idea
        // to have an "extending" bit that keeps two tasks from entering
        // extend_impl at the same time.
        //
        // Also we could enforce that `hop` is still what we expect it
        // to be at this point.

        // Did we get the right response?
        if msg.cmd() != RelayCmd::EXTENDED2 {
            return Err(Error::CircProto(format!(
                "wanted EXTENDED2; got {}",
                msg.cmd(),
            )));
        }

        // ???? Do we need to shutdown the circuit for the remaining error
        // ???? cases in this function?

        let msg = match msg {
            RelayMsg::Extended2(e) => e,
            _ => return Err(Error::InternalError("Body didn't match cmd".into())),
        };
        let relay_handshake = msg.into_body();

        trace!(
            "{}: Received EXTENDED2 cell; completing handshake.",
            self.unique_id
        );
        // Now perform the second part of the handshake, and see if it
        // succeeded.
        let keygen = H::client2(
            self.state
                .take()
                .expect("CircuitExtender::finish() called twice"),
            relay_handshake,
        )?;
        let layer = L::construct(keygen)?;

        debug!("{}: Handshake complete; circuit extended.", self.unique_id);

        // If we get here, it succeeded.  Add a new hop to the circuit.
        let (layer_fwd, layer_back) = layer.split();
        reactor.add_hop(
            self.supports_flowctrl_1,
            Box::new(layer_fwd),
            Box::new(layer_back),
            &self.params,
        );
        Ok(())
    }
}

/// Object to handle incoming cells and background tasks on a circuit
///
/// This type is returned when you finish a circuit; you need to spawn a
/// new task that calls `run()` on it.
#[must_use = "If you don't call run() on a reactor, the circuit won't work."]
pub struct Reactor {
    /// Receiver for control messages for this reactor, sent by `ClientCirc` objects.
    pub(super) control: mpsc::UnboundedReceiver<CtrlMsg>,
    /// Buffer for cells we can't send out the channel yet due to it being full.
    ///
    /// This should be used very very rarely: see `send_msg_direct`'s comments for more
    /// information. (in fact, using it will generate a warning!)
    pub(super) outbound: VecDeque<ChanCell>,
    /// The channel this circuit is using to send cells through.
    pub(super) channel: Channel,
    /// Input stream, on which we receive ChanMsg objects from this circuit's
    /// channel.
    // TODO: could use a SPSC channel here instead.
    pub(super) input: mpsc::Receiver<ClientCircChanMsg>,
    /// The cryptographic state for this circuit for inbound cells.
    /// This object is divided into multiple layers, each of which is
    /// shared with one hop of the circuit.
    pub(super) crypto_in: InboundClientCrypt,
    /// The cryptographic state for this circuit for outbound cells.
    pub(super) crypto_out: OutboundClientCrypt,
    /// List of hops state objects used by the reactor
    pub(super) hops: Vec<CircHop>,
    /// Shared atomic for the number of hops this circuit has.
    pub(super) num_hops: Arc<AtomicU8>,
    /// An identifier for logging about this reactor's circuit.
    pub(super) unique_id: UniqId,
    /// This circuit's identifier on the upstream channel.
    pub(super) channel_id: CircId,
    /// A handler for a meta cell, together with a result channel to notify on completion.
    pub(super) meta_handler: Option<(Box<dyn MetaCellHandler>, ReactorResultChannel<()>)>,
}

impl Reactor {
    /// Launch the reactor, and run until the circuit closes or we
    /// encounter an error.
    ///
    /// Once this method returns, the circuit is dead and cannot be
    /// used again.
    pub async fn run(mut self) -> Result<()> {
        trace!("{}: Running circuit reactor", self.unique_id);
        let result: Result<()> = loop {
            match self.run_once().await {
                Ok(()) => (),
                Err(ReactorError::Shutdown) => break Ok(()),
                Err(ReactorError::Err(e)) => break Err(e),
            }
        };
        debug!("{}: Circuit reactor stopped: {:?}", self.unique_id, result);
        result
    }

    /// Helper for run: doesn't mark the circuit closed on finish.  Only
    /// processes one cell or control message.
    pub(super) async fn run_once(&mut self) -> std::result::Result<(), ReactorError> {
        #[allow(clippy::cognitive_complexity)]
        let fut = futures::future::poll_fn(|cx| -> Poll<std::result::Result<_, ReactorError>> {
            let mut create_message = None;
            let mut did_things = false;

            // Check whether we've got a control message pending.
            if let Poll::Ready(ret) = Pin::new(&mut self.control).poll_next(cx) {
                match ret {
                    None => {
                        trace!("{}: reactor shutdown due to control drop", self.unique_id);
                        return Poll::Ready(Err(ReactorError::Shutdown));
                    }
                    Some(CtrlMsg::Shutdown) => {
                        trace!(
                            "{}: reactor shutdown due to explicit request",
                            self.unique_id
                        );
                        return Poll::Ready(Err(ReactorError::Shutdown));
                    }
                    // This message requires actually blocking, so we can't handle it inside
                    // this nonblocking poll_fn.
                    Some(x @ CtrlMsg::Create { .. }) => create_message = Some(x),
                    Some(msg) => {
                        self.handle_control(cx, msg)?;
                        did_things = true;
                    }
                }
            }

            // Check whether we've got an input message pending.
            if let Poll::Ready(ret) = Pin::new(&mut self.input).poll_next(cx) {
                match ret {
                    None => {
                        trace!("{}: reactor shutdown due to input drop", self.unique_id);
                        return Poll::Ready(Err(ReactorError::Shutdown));
                    }
                    Some(cell) => {
                        if self.handle_cell(cx, cell)? {
                            trace!("{}: reactor shutdown due to handled cell", self.unique_id);
                            return Poll::Ready(Err(ReactorError::Shutdown));
                        }
                        did_things = true;
                    }
                }
            }

            // Now for the tricky part. We want to grab some relay cells from all of our streams
            // and forward them on to the channel, but we need to pay attention to both whether
            // the channel can accept cells right now, and whether congestion control allows us
            // to send them.
            //
            // We also have to do somewhat cursed things and call start_send inside this poll_fn,
            // since we need to check whether the channel can still receive cells after each one
            // that we send.

            let mut streams_to_close = vec![];
            let mut stream_relaycells = vec![];

            // Is the channel ready to receive anything at all?
            if self.channel.poll_ready(cx)? {
                // (using this as a named block for early returns; not actually a loop)
                #[allow(clippy::never_loop)]
                'outer: loop {
                    // First, drain our queue of things we tried to send earlier, but couldn't.
                    while let Some(msg) = self.outbound.pop_front() {
                        trace!("{}: sending from enqueued: {:?}", self.unique_id, msg);
                        Pin::new(&mut self.channel).start_send(msg)?;

                        // `futures::Sink::start_send` dictates we need to call `poll_ready` before
                        // each `start_send` call.
                        if !self.channel.poll_ready(cx)? {
                            break 'outer;
                        }
                    }

                    // Let's look at our hops, and streams for each hop.
                    for (i, hop) in self.hops.iter_mut().enumerate() {
                        let hop_num = HopNum::from(i as u8);
                        // If we can, drain our queue of things we tried to send earlier, but
                        // couldn't due to congestion control.
                        if hop.sendwindow.window() > 0 {
                            'hop: while let Some((tag, msg)) = hop.outbound.pop_front() {
                                trace!(
                                    "{}: sending from hop-{}-enqueued: {:?}",
                                    self.unique_id,
                                    i,
                                    msg
                                );
                                Pin::new(&mut self.channel).start_send(msg)?;
                                hop.sendwindow.take(&tag)?;
                                if !self.channel.poll_ready(cx)? {
                                    break 'outer;
                                }
                                if hop.sendwindow.window() == 0 {
                                    break 'hop;
                                }
                            }
                        }
                        // Look at all of the streams on this hop.
                        for (id, stream) in hop.map.inner().iter_mut() {
                            if let StreamEnt::Open {
                                rx, send_window, ..
                            } = stream
                            {
                                // Do the stream and hop send windows allow us to obtain and
                                // send something?
                                //
                                // FIXME(eta): not everything counts toward congestion control!
                                if send_window.window() > 0 && hop.sendwindow.window() > 0 {
                                    match Pin::new(rx).poll_next(cx) {
                                        Poll::Ready(Some(m)) => {
                                            stream_relaycells
                                                .push((hop_num, RelayCell::new(*id, m)));
                                        }
                                        Poll::Ready(None) => {
                                            // Stream receiver was dropped; close the stream.
                                            // We can't close it here though due to borrowck; that
                                            // will happen later.
                                            streams_to_close.push((hop_num, *id));
                                        }
                                        Poll::Pending => {}
                                    }
                                }
                            }
                        }
                    }

                    break;
                }
            }

            // Close the streams we said we'd close.
            for (hopn, id) in streams_to_close {
                self.close_stream(cx, hopn, id)?;
                did_things = true;
            }
            // Send messages we said we'd send.
            for (hopn, rc) in stream_relaycells {
                self.send_relay_cell(cx, hopn, false, rc)?;
                did_things = true;
            }

            let _ = Pin::new(&mut self.channel)
                .poll_flush(cx)
                .map_err(|_| Error::ChannelClosed)?;
            if create_message.is_some() {
                Poll::Ready(Ok(create_message))
            } else if did_things {
                Poll::Ready(Ok(None))
            } else {
                Poll::Pending
            }
        });
        let create_message = fut.await?;
        if let Some(CtrlMsg::Create {
            recv_created,
            handshake,
            supports_authenticated_sendme,
            params,
            done,
        }) = create_message
        {
            let ret = match handshake {
                CircuitHandshake::CreateFast => {
                    self.create_firsthop_fast(recv_created, &params).await
                }
                CircuitHandshake::Ntor {
                    public_key,
                    ed_identity,
                } => {
                    self.create_firsthop_ntor(
                        recv_created,
                        ed_identity,
                        public_key,
                        supports_authenticated_sendme,
                        &params,
                    )
                    .await
                }
            };
            let _ = done.send(ret); // don't care if sender goes away
            futures::future::poll_fn(|cx| -> Poll<Result<()>> {
                let _ = Pin::new(&mut self.channel)
                    .poll_flush(cx)
                    .map_err(|_| Error::ChannelClosed)?;
                Poll::Ready(Ok(()))
            })
            .await?;
        }
        Ok(())
    }

    /// Helper: create the first hop of a circuit.
    ///
    /// This is parameterized not just on the RNG, but a wrapper object to
    /// build the right kind of create cell, a handshake object to perform
    /// the cryptographic cryptographic handshake, and a layer type to
    /// handle relay crypto after this hop is built.
    async fn create_impl<L, FWD, REV, H, W>(
        &mut self,
        recvcreated: oneshot::Receiver<CreateResponse>,
        wrap: &W,
        key: &H::KeyType,
        supports_flowctrl_1: bool,
        params: &CircParameters,
    ) -> Result<()>
    where
        L: CryptInit + ClientLayer<FWD, REV> + 'static + Send, // need all this?XXXX
        FWD: OutboundClientLayer + 'static + Send,
        REV: InboundClientLayer + 'static + Send,
        H: ClientHandshake,
        W: CreateHandshakeWrap,
        H::KeyGen: KeyGenerator,
    {
        // We don't need to shut down the circuit on failure here, since this
        // function consumes the PendingClientCirc and only returns
        // a ClientCirc on success.

        let (state, msg) = {
            // done like this because holding the RNG across an await boundary makes the future
            // non-Send
            let mut rng = rand::thread_rng();
            H::client1(&mut rng, key)?
        };
        let create_cell = wrap.to_chanmsg(msg);
        debug!(
            "{}: Extending to hop 1 with {}",
            self.unique_id,
            create_cell.cmd()
        );
        self.send_msg(create_cell).await?;

        let reply = recvcreated
            .await
            .map_err(|_| Error::CircProto("Circuit closed while waiting".into()))?;

        let relay_handshake = wrap.from_chanmsg(reply)?;
        let keygen = H::client2(state, relay_handshake)?;

        let layer = L::construct(keygen)?;

        debug!("{}: Handshake complete; circuit created.", self.unique_id);

        let (layer_fwd, layer_back) = layer.split();
        self.add_hop(
            supports_flowctrl_1,
            Box::new(layer_fwd),
            Box::new(layer_back),
            params,
        );
        Ok(())
    }

    /// Use the (questionable!) CREATE_FAST handshake to connect to the
    /// first hop of this circuit.
    ///
    /// There's no authentication in CREATE_FAST,
    /// so we don't need to know whom we're connecting to: we're just
    /// connecting to whichever relay the channel is for.
    async fn create_firsthop_fast(
        &mut self,
        recvcreated: oneshot::Receiver<CreateResponse>,
        params: &CircParameters,
    ) -> Result<()> {
        use crate::crypto::handshake::fast::CreateFastClient;
        let wrap = CreateFastWrap;
        self.create_impl::<Tor1RelayCrypto, _, _, CreateFastClient, _>(
            recvcreated,
            &wrap,
            &(),
            false,
            params,
        )
        .await
    }

    /// Use the ntor handshake to connect to the first hop of this circuit.
    ///
    /// Note that the provided 'target' must match the channel's target,
    /// or the handshake will fail.
    async fn create_firsthop_ntor(
        &mut self,
        recvcreated: oneshot::Receiver<CreateResponse>,
        ed_identity: pk::ed25519::Ed25519Identity,
        pubkey: NtorPublicKey,
        supports_flowctrl_1: bool,
        params: &CircParameters,
    ) -> Result<()> {
        // Exit now if we have an Ed25519 or RSA identity mismatch.
        // FIXME(eta): this is copypasta from Channel::check_match!
        if self.channel.peer_rsa_id() != &pubkey.id {
            return Err(Error::ChanMismatch(format!(
                "Identity {} does not match target {}",
                self.channel.peer_rsa_id(),
                pubkey.id,
            )));
        }
        if self.channel.peer_ed25519_id() != &ed_identity {
            return Err(Error::ChanMismatch(format!(
                "Identity {} does not match target {}",
                self.channel.peer_ed25519_id(),
                ed_identity
            )));
        }

        let wrap = Create2Wrap {
            handshake_type: 0x0002, // ntor
        };
        self.create_impl::<Tor1RelayCrypto, _, _, NtorClient, _>(
            recvcreated,
            &wrap,
            &pubkey,
            supports_flowctrl_1,
            params,
        )
        .await
    }

    /// Add a hop to the end of this circuit.
    fn add_hop(
        &mut self,
        supports_flowctrl_1: bool,
        fwd: Box<dyn OutboundClientLayer + 'static + Send>,
        rev: Box<dyn InboundClientLayer + 'static + Send>,
        params: &CircParameters,
    ) {
        let hop = crate::circuit::reactor::CircHop::new(
            supports_flowctrl_1,
            params.initial_send_window(),
        );
        self.hops.push(hop);
        self.crypto_in.add_layer(rev);
        self.crypto_out.add_layer(fwd);
        self.num_hops.fetch_add(1, Ordering::SeqCst);
    }

    /// Handle a RELAY cell on this circuit with stream ID 0.
    fn handle_meta_cell(&mut self, hopnum: HopNum, msg: RelayMsg) -> Result<()> {
        // SENDME cells and TRUNCATED get handled internally by the circuit.
        if let RelayMsg::Sendme(s) = msg {
            return self.handle_sendme(hopnum, s);
        }
        if let RelayMsg::Truncated(_) = msg {
            // XXXX need to handle Truncated cells. This isn't the right
            // way, but at least it's safe.
            // TODO: If we ever do handle Truncate cells more
            // correctly, we will need to audit all our use of HopNum
            // to identify a layer.  Otherwise we could confuse a
            // message from the previous hop N with a message from the
            // new hop N.
            return Err(Error::CircuitClosed);
        }

        trace!("{}: Received meta-cell {:?}", self.unique_id, msg);

        // For all other command types, we'll only get them in response
        // to another command, which should have registered a responder.
        //
        // TODO: that means that service-introduction circuits will need
        // a different implementation, but that should be okay. We'll work
        // something out.
        if let Some((mut handler, done)) = self.meta_handler.take() {
            if handler.expected_hop() == hopnum {
                // Somebody was waiting for a message -- maybe this message
                let ret = handler.finish(msg, self);
                trace!(
                    "{}: meta handler completed with result: {:?}",
                    self.unique_id,
                    ret
                );
                let _ = done.send(ret); // don't care if sender goes away
                Ok(())
            } else {
                // Somebody wanted a message from a different hop!  Put this
                // one back.
                self.meta_handler = Some((handler, done));
                Err(Error::CircProto(format!(
                    "Unexpected {} cell from hop {} on client circuit",
                    msg.cmd(),
                    hopnum,
                )))
            }
        } else {
            // No need to call shutdown here, since this error will
            // propagate to the reactor shut it down.
            Err(Error::CircProto(format!(
                "Unexpected {} cell on client circuit",
                msg.cmd()
            )))
        }
    }

    /// Handle a RELAY_SENDME cell on this circuit with stream ID 0.
    fn handle_sendme(&mut self, hopnum: HopNum, msg: Sendme) -> Result<()> {
        // No need to call "shutdown" on errors in this function;
        // it's called from the reactor task and errors will propagate there.
        let hop = self
            .hop_mut(hopnum)
            .ok_or_else(|| Error::CircProto(format!("Couldn't find {} hop", hopnum)))?;

        let auth: Option<[u8; 20]> = match msg.into_tag() {
            Some(v) if v.len() == 20 => {
                // XXXX ugly code.
                let mut tag = [0_u8; 20];
                (&mut tag).copy_from_slice(&v[..]);
                Some(tag)
            }
            Some(_) => return Err(Error::CircProto("malformed tag on circuit sendme".into())),
            None => {
                if !hop.auth_sendme_optional {
                    return Err(Error::CircProto("missing tag on circuit sendme".into()));
                } else {
                    None
                }
            }
        };
        match hop.sendwindow.put(auth) {
            Some(_) => Ok(()),
            None => Err(Error::CircProto("bad auth tag on circuit sendme".into())),
        }
    }

    /// Send a message onto the circuit's channel (to be called with a `Context`)
    ///
    /// If the channel is ready to accept messages, it will be sent immediately. If not, the message
    /// will be enqueued for sending at a later iteration of the reactor loop.
    ///
    /// # Note
    ///
    /// Making use of the enqueuing capabilities of this function is discouraged! You should first
    /// check whether the channel is ready to receive messages (`self.channel.poll_ready`), and
    /// ideally use this to implement backpressure (such that you do not read from other sources
    /// that would send here while you know you're unable to forward the messages on).
    fn send_msg_direct(&mut self, cx: &mut Context<'_>, msg: ChanMsg) -> Result<()> {
        let cell = ChanCell::new(self.channel_id, msg);
        if self.channel.poll_ready(cx)? {
            Pin::new(&mut self.channel).start_send(cell)?;
        } else {
            // This case shouldn't actually happen that often, if ever. We generally check whether
            // the channel can be sent to before calling this function (the one exception at the
            // time of writing is in circuit creation).
            //
            // If this is suddenly getting hit and it wasn't before, maybe you added something that
            // doesn't bother to check the channel (`self.channel.poll_ready(cx)`) before calling
            // this function, and that's getting used a lot?
            //
            // We don't want to drop cells on the floor, though, so this is good to have.
            warn!(
                "{}: having to enqueue cell due to backpressure: {:?}",
                self.unique_id, cell
            );
            self.outbound.push_back(cell);
        }
        Ok(())
    }

    /// Wrapper around `send_msg_direct` that uses `futures::future::poll_fn` to get a `Context`.
    async fn send_msg(&mut self, msg: ChanMsg) -> Result<()> {
        // HACK(eta): technically the closure passed to `poll_fn` is a `FnMut` closure, since it
        //            can be polled multiple times.
        //            We're going to return Ready immediately since we're only using `poll_fn` to
        //            get a `Context`, but the compiler doesn't know that, so use an `Option`
        //            which we can `take()` in order to move out of it.
        //            (if we do get polled again this'll panic, but that shouldn't happen!)
        let mut msg = Some(msg);
        futures::future::poll_fn(|cx| -> Poll<Result<()>> {
            self.send_msg_direct(cx, msg.take().expect("poll_fn called twice?"))?;
            Poll::Ready(Ok(()))
        })
        .await?;
        Ok(())
    }

    /// Encode the relay cell `cell`, encrypt it, and send it to the 'hop'th hop.
    ///
    /// Does not check whether the cell is well-formed or reasonable.
    fn send_relay_cell(
        &mut self,
        cx: &mut Context<'_>,
        hop: HopNum,
        early: bool,
        cell: RelayCell,
    ) -> Result<()> {
        let c_t_w = sendme::cell_counts_towards_windows(&cell);
        let mut body: RelayCellBody = cell.encode(&mut rand::thread_rng())?.into();
        let tag = self.crypto_out.encrypt(&mut body, hop)?;
        let msg = chancell::msg::Relay::from_raw(body.into());
        let msg = if early {
            ChanMsg::RelayEarly(msg)
        } else {
            ChanMsg::Relay(msg)
        };
        // If the cell counted towards our sendme window, decrement
        // that window, and maybe remember the authentication tag.
        if c_t_w {
            let hop_num = Into::<usize>::into(hop);
            let hop = &mut self.hops[hop_num];
            if hop.sendwindow.window() == 0 {
                let cell = ChanCell::new(self.channel_id, msg);
                // Send window is empty! Push this cell onto the hop's outbound queue, and it'll
                // get sent later.
                trace!(
                    "{}: having to use onto hop {} queue for cell: {:?}",
                    self.unique_id,
                    hop_num,
                    cell
                );
                hop.outbound.push_back((*tag, cell));
                return Ok(());
            }
            hop.sendwindow.take(tag)?;
        }
        self.send_msg_direct(cx, msg)
    }

    /// Handle a CtrlMsg other than Shutdown.
    fn handle_control(&mut self, cx: &mut Context<'_>, msg: CtrlMsg) -> Result<()> {
        trace!("{}: reactor received {:?}", self.unique_id, msg);
        match msg {
            // This is handled earlier, since it requires blocking.
            CtrlMsg::Create { .. } => panic!("got a CtrlMsg::Create in handle_control"),
            // This is handled earlier, since it requires generating a ReactorError.
            CtrlMsg::Shutdown => panic!("got a CtrlMsg::Shutdown in handle_control"),
            CtrlMsg::ExtendNtor {
                public_key,
                linkspecs,
                supports_authenticated_sendme,
                params,
                done,
            } => {
                match CircuitExtender::<NtorClient, Tor1RelayCrypto, _, _>::begin(
                    cx,
                    0x02,
                    &public_key,
                    linkspecs,
                    supports_authenticated_sendme,
                    params,
                    self,
                ) {
                    Ok(e) => {
                        self.meta_handler = Some((Box::new(e), done));
                    }
                    Err(e) => {
                        let _ = done.send(Err(e));
                    }
                };
            }
            CtrlMsg::BeginStream {
                hop_num,
                message,
                sender,
                rx,
                done,
            } => {
                let ret = self.begin_stream(cx, hop_num, message, sender, rx);
                let _ = done.send(ret); // don't care if sender goes away
            }
            CtrlMsg::SendSendme { stream_id, hop_num } => {
                let sendme = Sendme::new_empty();
                let cell = RelayCell::new(stream_id, sendme.into());
                self.send_relay_cell(cx, hop_num, false, cell)?;
            }
            #[cfg(test)]
            CtrlMsg::AddFakeHop {
                supports_flowctrl_1,
                fwd_lasthop,
                rev_lasthop,
                params,
                done,
            } => {
                use crate::circuit::test::DummyCrypto;

                let fwd = Box::new(DummyCrypto::new(fwd_lasthop));
                let rev = Box::new(DummyCrypto::new(rev_lasthop));
                self.add_hop(supports_flowctrl_1, fwd, rev, &params);
                let _ = done.send(Ok(()));
            }
            #[cfg(test)]
            CtrlMsg::QuerySendWindow { hop, done } => {
                let _ = done.send(if let Some(hop) = self.hop_mut(hop) {
                    Ok(hop.sendwindow.window_and_expected_tags())
                } else {
                    Err(Error::InternalError(
                        "received QuerySendWindow for unknown hop".into(),
                    ))
                });
            }
            #[cfg(test)]
            CtrlMsg::SendRelayCell { hop, early, cell } => {
                self.send_relay_cell(cx, hop, early, cell)?;
            }
        }
        Ok(())
    }

    /// Start a stream. Creates an entry in the stream map with the given channels, and sends the
    /// `message` to the provided hop.
    fn begin_stream(
        &mut self,
        cx: &mut Context<'_>,
        hopnum: HopNum,
        message: RelayMsg,
        sender: mpsc::Sender<RelayMsg>,
        rx: mpsc::Receiver<RelayMsg>,
    ) -> Result<StreamId> {
        let hop = self
            .hop_mut(hopnum)
            .ok_or_else(|| Error::InternalError(format!("No such hop {:?}", hopnum)))?;
        let send_window = StreamSendWindow::new(SEND_WINDOW_INIT);
        let r = hop.map.add_ent(sender, rx, send_window)?;
        let cell = RelayCell::new(r, message);
        self.send_relay_cell(cx, hopnum, false, cell)?;
        Ok(r)
    }

    /// Close the stream associated with `id` because the stream was
    /// dropped.
    ///
    /// If we have not already received an END cell on this stream, send one.
    fn close_stream(&mut self, cx: &mut Context<'_>, hopnum: HopNum, id: StreamId) -> Result<()> {
        // Mark the stream as closing.
        let hop = self.hop_mut(hopnum).ok_or_else(|| {
            Error::InternalError("Tried to close a stream on a hop that wasn't there?".into())
        })?;

        let should_send_end = hop.map.terminate(id)?;
        trace!(
            "{}: Ending stream {}; should_send_end={:?}",
            self.unique_id,
            id,
            should_send_end
        );
        // TODO: I am about 80% sure that we only send an END cell if
        // we didn't already get an END cell.  But I should double-check!
        if should_send_end == ShouldSendEnd::Send {
            let end_cell = RelayCell::new(id, End::new_misc().into());
            self.send_relay_cell(cx, hopnum, false, end_cell)?;
        }
        Ok(())
    }

    /// Helper: process a cell on a channel.  Most cells get ignored
    /// or rejected; a few get delivered to circuits.
    ///
    /// Return true if we should exit.
    fn handle_cell(&mut self, cx: &mut Context<'_>, cell: ClientCircChanMsg) -> Result<bool> {
        trace!("{}: handling cell: {:?}", self.unique_id, cell);
        use ClientCircChanMsg::*;
        match cell {
            Relay(r) => {
                self.handle_relay_cell(cx, r)?;
                Ok(false)
            }
            Destroy(_) => {
                self.handle_destroy_cell()?;
                Ok(true)
            }
        }
    }

    /// React to a Relay or RelayEarly cell.
    fn handle_relay_cell(&mut self, cx: &mut Context<'_>, cell: Relay) -> Result<()> {
        let mut body = cell.into_relay_body().into();

        // Decrypt the cell. If it's recognized, then find the
        // corresponding hop.
        let (hopnum, tag) = self.crypto_in.decrypt(&mut body)?;
        // Make a copy of the authentication tag. TODO: I'd rather not
        // copy it, but I don't see a way around it right now.
        let tag = {
            let mut tag_copy = [0_u8; 20];
            // XXXX This could crash if the tag length changes.  We'll
            // have to refactor it then.
            (&mut tag_copy).copy_from_slice(tag);
            tag_copy
        };
        // Decode the cell.
        let msg = RelayCell::decode(body.into())?;

        let c_t_w = sendme::cell_counts_towards_windows(&msg);

        // Decrement the circuit sendme windows, and see if we need to
        // send a sendme cell.
        let send_circ_sendme = if c_t_w {
            let hop = self
                .hop_mut(hopnum)
                .ok_or_else(|| Error::CircProto("Sendme from nonexistent hop".into()))?;
            hop.recvwindow.take()?
        } else {
            false
        };
        // If we do need to send a circuit-level SENDME cell, do so.
        if send_circ_sendme {
            let sendme = Sendme::new_tag(tag);
            let cell = RelayCell::new(0.into(), sendme.into());
            self.send_relay_cell(cx, hopnum, false, cell)?;
            self.hop_mut(hopnum)
                .ok_or_else(|| {
                    Error::InternalError("Trying to send SENDME to nonexistent hop".to_string())
                })?
                .recvwindow
                .put();
        }

        // Break the message apart into its streamID and message.
        let (streamid, msg) = msg.into_streamid_and_msg();

        // If this cell wants/refuses to have a Stream ID, does it
        // have/not have one?
        if !msg.cmd().accepts_streamid_val(streamid) {
            return Err(Error::CircProto(format!(
                "Invalid stream ID {} for relay command {}",
                streamid,
                msg.cmd()
            )));
        }

        // If this has a reasonable streamID value of 0, it's a meta cell,
        // not meant for a particular stream.
        if streamid.is_zero() {
            return self.handle_meta_cell(hopnum, msg);
        }

        let hop = self
            .hop_mut(hopnum)
            .ok_or_else(|| Error::CircProto("Cell from nonexistent hop!".into()))?;
        match hop.map.get_mut(streamid) {
            Some(StreamEnt::Open {
                sink,
                send_window,
                dropped,
                ..
            }) => {
                // The stream for this message exists, and is open.

                if let RelayMsg::Sendme(_) = msg {
                    // We need to handle sendmes here, not in the stream's
                    // recv() method, or else we'd never notice them if the
                    // stream isn't reading.
                    // FIXME(eta): I think ignoring the must_use return value here is okay, since
                    //             the tag is () anyway? or something???
                    let _ = send_window.put(Some(()));
                    return Ok(());
                }

                // Remember whether this was an end cell: if so we should
                // close the stream.
                let is_end_cell = matches!(msg, RelayMsg::End(_));

                // TODO: Add a wrapper type here to reject cells that should
                // never go to a client, like BEGIN.
                if let Err(e) = sink.try_send(msg) {
                    if e.is_full() {
                        // If we get here, we either have a logic bug (!), or an attacker
                        // is sending us more cells than we asked for via congestion control.
                        return Err(Error::CircProto(format!(
                            "Stream sink would block; received too many cells on stream ID {}",
                            streamid,
                        )));
                    }
                    if e.is_disconnected() && c_t_w {
                        // the other side of the stream has gone away; remember
                        // that we received a cell that we couldn't queue for it.
                        //
                        // Later this value will be recorded in a half-stream.
                        *dropped += 1;
                    }
                }
                if is_end_cell {
                    hop.map.end_received(streamid)?;
                }
            }
            Some(StreamEnt::EndSent(halfstream)) => {
                // We sent an end but maybe the other side hasn't heard.

                if matches!(msg, RelayMsg::End(_)) {
                    hop.map.end_received(streamid)?;
                } else {
                    halfstream.handle_msg(&msg)?;
                }
            }
            _ => {
                // No stream wants this message.
                return Err(Error::CircProto(
                    "Cell received on nonexistent stream!?".into(),
                ));
            }
        }
        Ok(())
    }

    /// Helper: process a destroy cell.
    #[allow(clippy::unnecessary_wraps)]
    fn handle_destroy_cell(&mut self) -> Result<()> {
        // I think there is nothing more to do here.
        Ok(())
    }

    /// Return the hop corresponding to `hopnum`, if there is one.
    fn hop_mut(&mut self, hopnum: HopNum) -> Option<&mut CircHop> {
        self.hops.get_mut(Into::<usize>::into(hopnum))
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        let _ = self.channel.close_circuit(self.channel_id);
    }
}

#[cfg(test)]
mod test {}
