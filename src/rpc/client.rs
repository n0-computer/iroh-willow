//! API for managing iroh spaces
//!
//! iroh spaces is an implementation of the [Willow] protocol.
//! The main entry point is the [`Client`].
//!
//! You obtain a [`Client`] via [`Iroh::spaces()`](crate::client::Iroh::spaces).
//!
//! [Willow]: https://willowprotocol.org/

// TODO: Reexport everything that is needed from iroh_willow.

use std::{
    collections::HashMap,
    pin::Pin,
    task::{ready, Context, Poll},
};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures_lite::{Stream, StreamExt};
use futures_util::{Sink, SinkExt};
use iroh::{NodeAddr, NodeId};
use iroh_blobs::Hash;
use quic_rpc::transport::ConnectionErrors;
use ref_cast::RefCast;
use serde::{Deserialize, Serialize};
use tokio_stream::{StreamMap, StreamNotifyClose};

use super::RpcClient;
use crate::{
    form::{AuthForm, SubspaceForm, TimestampForm},
    interest::{
        AreaOfInterestSelector, CapSelector, CapabilityPack, DelegateTo, Interests, RestrictArea,
    },
    proto::{
        data_model::{AuthorisedEntry, Path, SubspaceId},
        grouping::{Area, Range3d},
        keys::{NamespaceId, NamespaceKind, UserId},
        meadowcap::{AccessMode, SecretKey},
    },
    rpc::proto::*,
    session::{
        intents::{serde_encoding::Event, Completion, IntentUpdate},
        SessionInit, SessionMode,
    },
    store::traits::{StoreEvent, SubscribeParams},
};

/// Type alias for a memory-backed client.
pub type MemClient = Client<
    quic_rpc::transport::flume::FlumeConnector<
        crate::rpc::proto::Response,
        crate::rpc::proto::Request,
    >,
>;

/// Iroh Willow client.
#[derive(Debug, Clone, RefCast)]
#[repr(transparent)]
pub struct Client<C: quic_rpc::Connector<RpcService> = quic_rpc::client::BoxedConnector<RpcService>>
{
    pub(super) rpc: RpcClient<C>,
}

impl<C: quic_rpc::Connector<RpcService>> Client<C> {
    pub fn new(rpc: RpcClient<C>) -> Self {
        Self { rpc }
    }

    pub fn boxed(self) -> Client<quic_rpc::client::BoxedConnector<RpcService>>
    where
        C: quic_rpc::transport::boxed::BoxableConnector<
            crate::rpc::proto::Response,
            crate::rpc::proto::Request,
        >,
    {
        Client {
            rpc: self.rpc.boxed(),
        }
    }

    /// Create a new namespace in the Willow store.
    pub async fn create(&self, kind: NamespaceKind, owner: UserId) -> Result<Space<C>> {
        let req = CreateNamespaceRequest { kind, owner };
        let res = self.rpc.rpc(req).await.map_err(|e| anyhow::anyhow!(e))??;
        Ok(Space::new(self.rpc.clone(), res.0))
    }

    /// Create a new user in the Willow store.
    pub async fn create_user(&self) -> Result<UserId> {
        let req = CreateUserRequest;
        let res: CreateUserResponse = self.rpc.rpc(req).await??;
        Ok(res.0)
    }

    /// Delegate capabilities to another user.
    ///
    /// Returns a `Vec` of [`CapabilityPack`]s, which can be serialized.
    pub async fn delegate_caps(
        &self,
        from: CapSelector,
        access_mode: AccessMode,
        to: DelegateTo,
    ) -> Result<Vec<CapabilityPack>> {
        let req = DelegateCapsRequest {
            from,
            access_mode,
            to,
        };
        let res = self.rpc.rpc(req).await??;
        Ok(res.0)
    }

    /// Import capabilities.
    pub async fn import_caps(&self, caps: Vec<CapabilityPack>) -> Result<()> {
        let req = ImportCapsRequest { caps };
        self.rpc.rpc(req).await??;
        Ok(())
    }

    /// Import a ticket and start to synchronize.
    pub async fn import_and_sync(
        &self,
        ticket: SpaceTicket,
        mode: SessionMode,
    ) -> Result<(Space<C>, SyncHandleSet)> {
        if ticket.caps.is_empty() {
            anyhow::bail!("Invalid ticket: Does not include any capabilities");
        }
        let mut namespaces = ticket.caps.iter().map(|pack| pack.namespace());
        let namespace = namespaces.next().expect("just checked");
        if !namespaces.all(|n| n == namespace) {
            anyhow::bail!("Invalid ticket: Capabilities do not all refer to the same namespace");
        }

        self.import_caps(ticket.caps).await?;
        let interests = Interests::builder().add_full_cap(CapSelector::any(namespace));
        let init = SessionInit::new(interests, mode);
        let mut intents = SyncHandleSet::default();
        for addr in ticket.nodes {
            let node_id = addr.node_id;
            self.add_node_addr(addr).await?;
            let intent = self.sync_with_peer(node_id, init.clone()).await?;
            intents.insert(node_id, intent)?;
        }
        let space = Space::new(self.rpc.clone(), namespace);
        Ok((space, intents))
    }

    /// Synchronize with a peer.
    pub async fn sync_with_peer(&self, peer: NodeId, init: SessionInit) -> Result<SyncHandle> {
        let req = SyncWithPeerRequest { peer, init };
        let (update_tx, event_rx) = self.rpc.bidi(req).await?;

        let update_tx: UpdateSender = Box::pin(
            update_tx
                .with(|update| async move { Ok(SyncWithPeerUpdate(update)) })
                .sink_map_err(|e: <C as ConnectionErrors>::SendError| e.into()),
        );

        let event_rx = Box::pin(event_rx.map(|res| match res {
            Ok(Ok(SyncWithPeerResponse::Event(event))) => event,
            Ok(Ok(SyncWithPeerResponse::Started)) => Event::ReconciledAll, // or another appropriate event
            Err(e) => Event::Abort {
                error: e.to_string(),
            },
            Ok(Err(e)) => Event::Abort {
                error: e.to_string(),
            },
        }));

        Ok(SyncHandle::new(update_tx, event_rx, Default::default()))
    }

    /// Import a secret into the Willow store.
    pub async fn import_secret(&self, secret: impl Into<SecretKey>) -> Result<()> {
        let req = InsertSecretRequest {
            secret: secret.into(),
        };
        self.rpc.rpc(req).await??;
        Ok(())
    }

    /// Fetches the [`NodeAddr`] for this node.
    ///
    /// See also [`Endpoint::node_addr`](iroh::Endpoint::node_addr).
    pub async fn node_addr(&self) -> Result<NodeAddr> {
        let addr = self.rpc.rpc(AddrRequest).await??;
        Ok(addr)
    }

    /// Adds a known node address to this node.
    ///
    /// See also [`Endpoint::add_node_addr`](iroh::Endpoint::add_node_addr).
    pub async fn add_node_addr(&self, addr: NodeAddr) -> Result<()> {
        self.rpc.rpc(AddAddrRequest { addr }).await??;
        Ok(())
    }
}

/// A space to store entries in.
#[derive(Debug, Clone)]
pub struct Space<C: quic_rpc::Connector<RpcService> = quic_rpc::client::BoxedConnector<RpcService>>
{
    rpc: RpcClient<C>,
    namespace_id: NamespaceId,
}

impl<C: quic_rpc::Connector<RpcService>> Space<C> {
    fn new(rpc: RpcClient<C>, namespace_id: NamespaceId) -> Self {
        Self { rpc, namespace_id }
    }

    fn spaces(&self) -> &Client<C> {
        Client::ref_cast(&self.rpc)
    }

    /// Returns the identifier for this space.
    pub fn namespace_id(&self) -> NamespaceId {
        self.namespace_id
    }

    async fn insert(&self, entry: EntryForm, payload: PayloadForm) -> Result<InsertEntrySuccess> {
        let form = FullEntryForm {
            namespace_id: self.namespace_id,
            subspace_id: entry.subspace_id,
            path: entry.path,
            timestamp: entry.timestamp,
            payload,
        };
        let auth = entry.auth;
        let req = InsertEntryRequest { entry: form, auth };
        let res = self.rpc.rpc(req).await??;
        Ok(res)
    }

    /// Inserts a new entry, with the payload set to the hash of a blob.
    ///
    /// Note that the payload must exist in the local blob store, otherwise the operation will fail.
    pub async fn insert_hash(&self, entry: EntryForm, payload: Hash) -> Result<InsertEntrySuccess> {
        let payload = PayloadForm::Checked(payload);
        self.insert(entry, payload).await
    }

    /// Inserts a new entry, with the payload imported from a byte string.
    pub async fn insert_bytes(
        &self,
        blobs: &impl iroh_blobs::store::Store,
        entry: EntryForm,
        payload: impl Into<Bytes>,
    ) -> Result<InsertEntrySuccess> {
        let tag = blobs
            .import_bytes(payload.into(), iroh_blobs::BlobFormat::Raw)
            .await?;
        self.insert_hash(entry, *tag.hash()).await
    }

    // TODO(matheus23): figure out how to use blobs
    // /// Inserts a new entry, with the payload imported from a byte reader.
    // pub async fn insert_reader(
    //     &self,
    //     entry: EntryForm,
    //     payload: impl AsyncRead + Send + Unpin + 'static,
    // ) -> Result<InsertEntrySuccess> {
    //     let batch = self.blobs().batch().await?;
    //     let tag = batch.add_reader(payload).await?;
    //     self.insert_hash(entry, *tag.hash()).await
    // }

    // /// Inserts a new entry, with the payload imported from a byte stream.
    // pub async fn insert_stream(
    //     &self,
    //     entry: EntryForm,
    //     payload: impl Stream<Item = std::io::Result<Bytes>> + Send + Unpin + 'static,
    // ) -> Result<InsertEntrySuccess> {
    //     let batch = self.blobs().batch().await?;
    //     let tag = batch.add_stream(payload).await?;
    //     self.insert_hash(entry, *tag.hash()).await
    // }

    // /// Inserts a new entry, with the payload imported from a file.
    // pub async fn insert_from_file(
    //     &self,
    //     entry: EntryForm,
    //     file_path: PathBuf,
    // ) -> Result<InsertEntrySuccess> {
    //     let batch = self.blobs().batch().await?;
    //     let (tag, _len) = batch.add_file(file_path).await?;
    //     self.insert_hash(entry, *tag.hash()).await
    // }

    /// Ingest an authorised entry.
    // TODO: Not sure if we should expose this on the client at all.
    pub async fn ingest(&self, authorised_entry: AuthorisedEntry) -> Result<()> {
        let req = IngestEntryRequest { authorised_entry };
        self.rpc.rpc(req).await??;
        Ok(())
    }

    /// Get a single entry.
    pub async fn get_one(
        &self,
        subspace: SubspaceId,
        path: Path,
    ) -> Result<Option<AuthorisedEntry>> {
        let req = GetEntryRequest {
            namespace: self.namespace_id,
            subspace,
            path,
        };
        let entry = self.rpc.rpc(req).await??;
        Ok(entry.0.map(Into::into))
    }

    /// Get entries by range.
    pub async fn get_many(
        &self,
        range: Range3d,
    ) -> Result<impl Stream<Item = Result<AuthorisedEntry>>> {
        let req = GetEntriesRequest {
            namespace: self.namespace_id,
            range,
        };
        let stream = self.rpc.try_server_streaming(req).await?;
        Ok(stream.map(|res| res.map(|r| r.0).map_err(anyhow::Error::from)))
    }

    /// Syncs with a peer and quit the session after a single reconciliation of the selected areas.
    ///
    /// Returns an [`SyncHandle`] that emits events for the reconciliation. If you want to wait for everything to complete,
    /// await [`SyncHandle::complete`].
    ///
    /// This will connect to the node, start a sync session, and submit all our capabilities for this namespace,
    /// constrained to the selected areas.
    ///
    /// If you want to specify the capabilities to submit more concretely, use [`Client::sync_with_peer`].
    pub async fn sync_once(
        &self,
        node: NodeId,
        areas: AreaOfInterestSelector,
    ) -> Result<SyncHandle> {
        let cap = CapSelector::any(self.namespace_id);
        let interests = Interests::builder().add(cap, areas);
        let init = SessionInit::reconcile_once(interests);
        self.spaces().sync_with_peer(node, init).await
    }

    /// Sync with a peer and keep sending and receiving live updates for the selected areas.
    ///
    /// Returns an [`SyncHandle`] that emits events for the reconciliation. If you want to wait for everything to complete,
    /// await [`SyncHandle::complete`].
    ///
    /// This will connect to the node, start a sync session, and submit all our capabilities for this namespace,
    /// constrained to the selected areas.
    ///
    /// If you want to specify the capabilities to submit more concretely, use [`Client::sync_with_peer`].
    pub async fn sync_continuously(
        &self,
        node: NodeId,
        areas: AreaOfInterestSelector,
    ) -> Result<SyncHandle> {
        let cap = CapSelector::any(self.namespace_id);
        let interests = Interests::builder().add(cap, areas);
        let init = SessionInit::continuous(interests);
        self.spaces().sync_with_peer(node, init).await
    }

    /// Share access to this space, or parts of this space, with another user.
    ///
    /// This will use any matching capability as the source of the capability delegation.
    /// If you want to specify more options, use [`Client::delegate_caps`].
    pub async fn share(
        &self,
        receiver: UserId,
        access_mode: AccessMode,
        restrict_area: RestrictArea,
    ) -> Result<SpaceTicket> {
        let caps = self
            .spaces()
            .delegate_caps(
                CapSelector::any(self.namespace_id),
                access_mode,
                DelegateTo::new(receiver, restrict_area),
            )
            .await?;
        let node_addr = self.spaces().node_addr().await?;
        Ok(SpaceTicket {
            caps,
            nodes: vec![node_addr],
        })
    }

    /// Subscribe to events concerning entries included by an `Area`.
    pub async fn subscribe_area(
        &self,
        area: Area,
        params: SubscribeParams,
    ) -> Result<impl Stream<Item = Result<StoreEvent>>> {
        let req = SubscribeRequest {
            namespace: self.namespace_id,
            area,
            params,
            initial_progress_id: None,
        };
        let stream = self.rpc.try_server_streaming(req).await?;
        let stream = stream.map(|item| item.map_err(anyhow::Error::from));
        Ok(stream)
    }

    /// Resume a subscription using a progress ID obtained from a previous subscription.
    pub async fn resume_subscription(
        &self,
        progress_id: u64,
        area: Area,
        params: SubscribeParams,
    ) -> Result<impl Stream<Item = Result<StoreEvent>>> {
        let req = SubscribeRequest {
            namespace: self.namespace_id,
            area,
            params,
            initial_progress_id: Some(progress_id),
        };
        let stream = self.rpc.try_server_streaming(req).await?;
        let stream = stream.map(|item| item.map_err(anyhow::Error::from));
        Ok(stream)
    }
}

/// A ticket to import and sync a space.
#[derive(Debug, Serialize, Deserialize)]
pub struct SpaceTicket {
    /// Capabilities for a space.
    pub caps: Vec<CapabilityPack>,
    /// List of nodes to sync with.
    pub nodes: Vec<NodeAddr>,
}

/// Handle to a synchronization intent.
///
/// The `SyncHandle` is a `Stream` of [`Event`]s. It *must* be progressed in a loop,
/// otherwise the session will be blocked from progressing.
///
/// The `SyncHandle` can also submit new interests into the session.
// This version of SyncHandle differs from the one in iroh-willow intents module
// by using the Event type instead of EventKind, which serializes the error to a string
// to cross the RPC boundary. Maybe look into making the main iroh_willow Error type
// serializable instead.
#[derive(derive_more::Debug)]
pub struct SyncHandle {
    #[debug("UpdateSender")]
    update_tx: UpdateSender,
    #[debug("EventReceiver")]
    event_rx: EventReceiver,
    state: SyncProgress,
}

/// Sends updates into a reconciliation intent.
///
/// Can be obtained from [`SyncHandle::split`].
pub type UpdateSender = Pin<Box<dyn Sink<IntentUpdate, Error = anyhow::Error> + Send + 'static>>;

/// Receives events for a reconciliation intent.
///
/// Can be obtained from [`SyncHandle::split`].
pub type EventReceiver = Pin<Box<dyn Stream<Item = Event> + Send + 'static>>;

impl SyncHandle {
    /// Creates a new `SyncHandle` with the given update sender and event receiver.
    fn new(update_tx: UpdateSender, event_rx: EventReceiver, state: SyncProgress) -> Self {
        Self {
            update_tx,
            event_rx,
            state,
        }
    }

    /// Splits the `SyncHandle` into a update sender sink and event receiver stream.
    ///
    /// The intent will be dropped once both the sender and receiver are dropped.
    pub fn split(self) -> (UpdateSender, EventReceiver) {
        (self.update_tx, self.event_rx)
    }

    /// Waits for the intent to be completed.
    ///
    /// This future completes either if the session terminated, or if all interests of the intent
    /// are reconciled and the intent is not in live data mode.
    ///
    /// Note that successful completion of this future does not guarantee that all interests were
    /// fulfilled.
    pub async fn complete(&mut self) -> Result<Completion> {
        let mut state = SyncProgress::default();
        while let Some(event) = self.event_rx.next().await {
            state.handle_event(&event);
            if state.is_ready() {
                break;
            }
        }
        state.into_completion()
    }

    /// Submit new synchronisation interests into the session.
    ///
    /// The `SyncHandle` will then receive events for these interests in addition to already
    /// submitted interests.
    pub async fn add_interests(&mut self, interests: impl Into<Interests>) -> Result<()> {
        self.update_tx
            .send(IntentUpdate::AddInterests(interests.into()))
            .await?;
        Ok(())
    }

    // TODO: I think all should work via dropping, but let's make sure that is the case.
    // /// Close the intent.
    // pub async fn close(&mut self) {
    //     self.update_tx.send(IntentUpdate::Close).await.ok();
    // }
}

impl Stream for SyncHandle {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(match ready!(Pin::new(&mut self.event_rx).poll_next(cx)) {
            None => None,
            Some(event) => {
                self.state.handle_event(&event);
                Some(event)
            }
        })
    }
}

/// Completion state for a [`SyncHandle`].
#[derive(Debug, Default)]
pub struct SyncProgress {
    partial: bool,
    complete: bool,
    failed: Option<String>,
}
impl SyncProgress {
    fn handle_event(&mut self, event: &Event) {
        match event {
            Event::ReconciledAll => self.complete = true,
            Event::Reconciled { .. } => self.partial = true,
            Event::Abort { error } => self.failed = Some(error.clone()),
            _ => {}
        }
    }

    fn is_ready(&self) -> bool {
        self.complete || self.failed.is_some()
    }

    fn into_completion(self) -> Result<Completion> {
        if let Some(error) = self.failed {
            Err(anyhow!(error))
        } else if self.complete {
            Ok(Completion::Complete)
        } else if self.partial {
            Ok(Completion::Partial)
        } else {
            Ok(Completion::Nothing)
        }
    }
}

/// Merges synchronisation intent handles into one struct.
#[derive(Default, derive_more::Debug)]
#[debug("MergedSyncHandle({:?})", self.event_rx.keys().collect::<Vec<_>>())]
pub struct SyncHandleSet {
    event_rx: StreamMap<NodeId, StreamNotifyClose<EventReceiver>>,
    intents: HashMap<NodeId, HandleState>,
}

#[derive(derive_more::Debug)]
struct HandleState {
    #[debug("UpdateSender")]
    update_tx: UpdateSender,
    state: SyncProgress,
}

impl SyncHandleSet {
    /// Add a sync intent to the set.
    ///
    /// Returns an error if there is already a sync intent for this peer in the set.
    pub fn insert(&mut self, peer: NodeId, handle: SyncHandle) -> Result<(), IntentExistsError> {
        if let std::collections::hash_map::Entry::Vacant(e) = self.intents.entry(peer) {
            let SyncHandle {
                update_tx,
                event_rx,
                state,
            } = handle;
            self.event_rx.insert(peer, StreamNotifyClose::new(event_rx));
            e.insert(HandleState { update_tx, state });
            Ok(())
        } else {
            Err(IntentExistsError(peer))
        }
    }

    /// Removes a sync intent from the set.
    pub fn remove(&mut self, peer: &NodeId) -> Option<SyncHandle> {
        self.event_rx.remove(peer).and_then(|event_rx| {
            self.intents.remove(peer).map(|state| {
                SyncHandle::new(
                    state.update_tx,
                    event_rx.into_inner().expect("unreachable"),
                    state.state,
                )
            })
        })
    }

    /// Submit new synchronisation interests into all sessions.
    pub async fn add_interests(&mut self, interests: impl Into<Interests>) -> Result<()> {
        let interests: Interests = interests.into();
        let futs = self.intents.values_mut().map(|intent| {
            intent
                .update_tx
                .send(IntentUpdate::AddInterests(interests.clone()))
        });
        futures_buffered::try_join_all(futs).await?;
        Ok(())
    }

    /// Wait for all intents to complete.
    pub async fn complete_all(mut self) -> HashMap<NodeId, Result<Completion>> {
        let futs = self.intents.drain().map(|(node_id, state)| {
            let event_rx = self
                .event_rx
                .remove(&node_id)
                .expect("unreachable")
                .into_inner()
                .expect("unreachable");
            async move {
                let res = SyncHandle::new(state.update_tx, event_rx, state.state)
                    .complete()
                    .await;
                (node_id, res)
            }
        });
        let res = futures_buffered::join_all(futs).await;
        res.into_iter().collect()
    }
}

impl Stream for SyncHandleSet {
    type Item = (NodeId, Event);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match ready!(Pin::new(&mut self.event_rx).poll_next(cx)) {
                None => break Poll::Ready(None),
                Some((peer, Some(event))) => break Poll::Ready(Some((peer, event))),
                Some((peer, None)) => {
                    self.intents.remove(&peer);
                    self.event_rx.remove(&peer);
                    continue;
                }
            }
        }
    }
}

/// Error returned when trying to insert a [`SyncHandle`] into a [`SyncHandleSet] for a peer that is already in the set.
#[derive(Debug, thiserror::Error)]
#[error("The set already contains a sync intent for this peer.")]
pub struct IntentExistsError(pub NodeId);

/// Form to insert a new entry
#[derive(Debug)]
pub struct EntryForm {
    /// The authorisation, either an exact capability, or a user id to select a capability for automatically.
    pub auth: AuthForm,
    /// The subspace, either exact or automatically set to the authorising user.
    pub subspace_id: SubspaceForm,
    /// The path
    pub path: Path,
    /// The timestamp, either exact or automatically set current time.
    pub timestamp: TimestampForm,
}

impl EntryForm {
    /// Creates a new entry form with the specified user and path.
    ///
    /// The subspace will be set to the specified user id.
    /// The timestamp will be set to the current system time.
    /// To authorise the entry, any applicable capability issued to the specified user id
    /// that covers this path will be used, or return an error if no such capability is available.
    pub fn new(user: UserId, path: Path) -> Self {
        Self {
            auth: AuthForm::Any(user),
            path,
            subspace_id: Default::default(),
            timestamp: Default::default(),
        }
    }

    // TODO: Add builder methods for auth, subspace_id, timestamp
}
