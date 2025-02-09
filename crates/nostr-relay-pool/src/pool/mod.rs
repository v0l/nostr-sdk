// Copyright (c) 2022-2023 Yuki Kishimoto
// Copyright (c) 2023-2024 Rust Nostr Developers
// Distributed under the MIT software license

//! Relay Pool

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_utility::futures_util::{future, StreamExt};
use atomic_destructor::{AtomicDestructor, StealthClone};
use nostr_database::prelude::*;
use tokio::sync::broadcast;

pub mod constants;
mod error;
mod inner;
pub mod options;
mod output;

pub use self::error::Error;
use self::inner::InnerRelayPool;
pub use self::options::RelayPoolOptions;
pub use self::output::Output;
use crate::relay::flags::FlagCheck;
use crate::relay::options::{RelayOptions, ReqExitPolicy, SyncOptions};
use crate::relay::{Relay, RelayFiltering, RelayStatus};
use crate::shared::SharedState;
use crate::stream::ReceiverStream;
use crate::{Reconciliation, RelayServiceFlags, SubscribeOptions};

/// Relay Pool Notification
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayPoolNotification {
    /// Received an [`Event`]. Does not include events sent by this client.
    Event {
        /// Relay url
        relay_url: RelayUrl,
        /// Subscription ID
        subscription_id: SubscriptionId,
        /// Event
        event: Box<Event>,
    },
    /// Received a [`RelayMessage`]. Includes messages wrapping events that were sent by this client.
    Message {
        /// Relay url
        relay_url: RelayUrl,
        /// Relay Message
        message: RelayMessage,
    },
    /// Relay status changed
    #[deprecated(since = "0.37.0")]
    RelayStatus {
        /// Relay url
        relay_url: RelayUrl,
        /// Relay Status
        status: RelayStatus,
    },
    /// Authenticated to relay
    ///
    /// <https://github.com/nostr-protocol/nips/blob/master/42.md>
    #[deprecated(since = "0.38.0")]
    Authenticated {
        /// Relay url
        relay_url: RelayUrl,
    },
    /// Shutdown
    ///
    /// This notification variant is sent after [`RelayPool::shutdown`] method is called and all connections have been closed.
    Shutdown,
}

/// Relay Pool
#[derive(Debug, Clone)]
pub struct RelayPool {
    inner: AtomicDestructor<InnerRelayPool>,
}

impl Default for RelayPool {
    fn default() -> Self {
        Self::new(RelayPoolOptions::default())
    }
}

impl StealthClone for RelayPool {
    fn stealth_clone(&self) -> Self {
        Self {
            inner: self.inner.stealth_clone(),
        }
    }
}

impl RelayPool {
    /// Create new `RelayPool`
    #[inline]
    pub fn new(opts: RelayPoolOptions) -> Self {
        Self::__with_shared_state(opts, SharedState::default())
    }

    #[inline]
    #[doc(hidden)]
    pub fn __with_shared_state(opts: RelayPoolOptions, state: SharedState) -> Self {
        Self {
            inner: AtomicDestructor::new(InnerRelayPool::new(opts, state)),
        }
    }

    /// Completely shutdown pool
    ///
    /// This method disconnects and removes all relays from the [`RelayPool`] and then
    /// sends [`RelayPoolNotification::Shutdown`] notification.
    ///
    /// After this method has been called, the [`RelayPool`] can no longer be used (i.e. can't add relays).
    #[inline]
    pub async fn shutdown(&self) -> Result<(), Error> {
        self.inner.shutdown().await
    }

    /// Get new **pool** notification listener
    ///
    /// <div class="warning">When you call this method, you subscribe to the notifications channel from that precise moment. Anything received by relay/s before that moment is not included in the channel!</div>
    pub fn notifications(&self) -> broadcast::Receiver<RelayPoolNotification> {
        self.inner.notification_sender.subscribe()
    }

    /// Get shared state
    #[inline]
    pub fn state(&self) -> &SharedState {
        &self.inner.state
    }

    /// Get database
    #[inline]
    pub fn database(&self) -> &Arc<dyn NostrDatabase> {
        self.inner.state.database()
    }

    /// Get relay filtering
    #[inline]
    pub fn filtering(&self) -> &RelayFiltering {
        self.inner.state.filtering()
    }

    /// Get all relays
    ///
    /// This method returns all relays added to the pool, including the ones for gossip protocol or other services.
    pub async fn all_relays(&self) -> HashMap<RelayUrl, Relay> {
        let relays = self.inner.atomic.relays.read().await;
        relays.clone()
    }

    /// Get relays with `READ` or `WRITE` flags
    pub async fn relays(&self) -> HashMap<RelayUrl, Relay> {
        self.relays_with_flag(
            RelayServiceFlags::READ | RelayServiceFlags::WRITE,
            FlagCheck::Any,
        )
        .await
    }

    /// Get relays that have a certain [RelayServiceFlag] enabled
    pub async fn relays_with_flag(
        &self,
        flag: RelayServiceFlags,
        check: FlagCheck,
    ) -> HashMap<RelayUrl, Relay> {
        let relays = self.inner.atomic.relays.read().await;
        self.inner
            .internal_relays_with_flag(&relays, flag, check)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Get [`Relay`]
    #[inline]
    pub async fn relay<U>(&self, url: U) -> Result<Relay, Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        self.inner.relay(url).await
    }

    /// Add new relay
    ///
    /// If are set pool subscriptions, the new added relay will inherit them.
    /// Use [`RelayPool::subscribe_to`] method instead of [`RelayPool::subscribe`],
    /// to avoid to set pool subscriptions.
    ///
    /// Connection is **NOT** automatically started with relay, remember to call [`RelayPool::connect`] or [`RelayPool::connect_relay`]!
    #[inline]
    pub async fn add_relay<U>(&self, url: U, opts: RelayOptions) -> Result<bool, Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        self.inner.add_relay(url, true, opts).await
    }

    // Private API
    //
    // Try to get relay by `url` or add it to pool.
    // Return `Some(..)` only if the relay already exists.
    #[inline]
    #[doc(hidden)]
    pub async fn __get_or_add_relay(
        &self,
        url: RelayUrl,
        inherit_pool_subscriptions: bool,
        opts: RelayOptions,
    ) -> Result<Option<Relay>, Error> {
        self.inner
            .get_or_add_relay(url, inherit_pool_subscriptions, opts)
            .await
    }

    /// Remove and disconnect relay
    ///
    /// If the relay has [`RelayServiceFlags::GOSSIP`], it will not be removed from the pool and its
    /// flags will be updated (remove [`RelayServiceFlags::READ`],
    /// [`RelayServiceFlags::WRITE`] and [`RelayServiceFlags::DISCOVERY`] flags).
    ///
    /// To fore remove a relay use [`RelayPool::force_remove_relay`].
    #[inline]
    pub async fn remove_relay<U>(&self, url: U) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        self.inner.remove_relay(url, false).await
    }

    /// Force remove and disconnect relay
    ///
    /// Note: this method will remove the relay, also if it's in use for the gossip model or other service!
    #[inline]
    pub async fn force_remove_relay<U>(&self, url: U) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        self.inner.remove_relay(url, true).await
    }

    /// Disconnect and remove all relays
    ///
    /// This method may not remove all relays.
    /// Use [`RelayPool::force_remove_all_relays`] to remove every relay.
    #[inline]
    pub async fn remove_all_relays(&self) -> Result<(), Error> {
        self.inner.remove_all_relays(false).await
    }

    /// Disconnect and force remove all relays
    #[inline]
    pub async fn force_remove_all_relays(&self) -> Result<(), Error> {
        self.inner.remove_all_relays(true).await
    }

    /// Connect to all added relays
    pub async fn connect(&self) {
        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Connect
        for relay in relays.values() {
            relay.connect()
        }
    }

    /// Waits for relays connections
    ///
    /// Wait for relays connections at most for the specified `timeout`.
    /// The code continues when the relays are connected or the `timeout` is reached.
    pub async fn wait_for_connection(&self, timeout: Duration) {
        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Compose futures
        let mut futures = Vec::with_capacity(relays.len());
        for relay in relays.values() {
            futures.push(relay.wait_for_connection(timeout));
        }

        // Join futures
        future::join_all(futures).await;
    }

    /// Try to establish a connection with the relays.
    ///
    /// Attempts to establish a connection without spawning the connection task if it fails.
    /// This means that if the connection fails, no automatic retries are scheduled.
    /// Use [`RelayPool::connect`] if you want to immediately spawn a connection task,
    /// regardless of whether the initial connection succeeds.
    ///
    /// For further details, see the documentation of [`Relay::try_connect`].
    pub async fn try_connect(&self, timeout: Duration) -> Output<()> {
        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(relays.len());
        let mut futures = Vec::with_capacity(relays.len());
        let mut output: Output<()> = Output::default();

        // Filter only relays that can connect and compose futures
        for relay in relays.values().filter(|r| r.status().can_connect()) {
            urls.push(relay.url().clone());
            futures.push(relay.try_connect(timeout));
        }

        // TODO: use semaphore to limit number concurrent connections?

        // Join futures
        let list = future::join_all(futures).await;

        // Iterate results and compose output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(..) => {
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        output
    }

    /// Disconnect from all relays
    pub async fn disconnect(&self) -> Result<(), Error> {
        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Iter values and disconnect
        for relay in relays.values() {
            relay.disconnect()?;
        }

        Ok(())
    }

    /// Connect to a previously added relay
    ///
    /// This method doesn't provide any information on if the connection was successful or not.
    ///
    /// Return [`Error::RelayNotFound`] if the relay doesn't exist in the pool.
    pub async fn connect_relay<U>(&self, url: U) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert url
        let url: RelayUrl = url.try_into_url()?;

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Get relay
        let relay: &Relay = self.inner.internal_relay(&relays, &url)?;

        // Connect
        relay.connect();

        Ok(())
    }

    /// Try to connect to a previously added relay
    ///
    /// For further details, see the documentation of [`Relay::try_connect`].
    pub async fn try_connect_relay<U>(&self, url: U, timeout: Duration) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert url
        let url: RelayUrl = url.try_into_url()?;

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Get relay
        let relay: &Relay = self.inner.internal_relay(&relays, &url)?;

        // Try to connect
        relay.try_connect(timeout).await?;

        Ok(())
    }

    /// Disconnect relay
    pub async fn disconnect_relay<U>(&self, url: U) -> Result<(), Error>
    where
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Convert url
        let url: RelayUrl = url.try_into_url()?;

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Get relay
        let relay: &Relay = self.inner.internal_relay(&relays, &url)?;

        // Disconnect
        relay.disconnect()?;

        Ok(())
    }

    /// Get subscriptions
    #[inline]
    pub async fn subscriptions(&self) -> HashMap<SubscriptionId, Vec<Filter>> {
        self.inner.subscriptions().await
    }

    /// Get subscription
    #[inline]
    pub async fn subscription(&self, id: &SubscriptionId) -> Option<Vec<Filter>> {
        self.inner.subscription(id).await
    }

    /// Register subscription in the [RelayPool]
    ///
    /// When a new relay will be added, saved subscriptions will be automatically used for it.
    #[inline]
    pub async fn save_subscription(&self, id: SubscriptionId, filters: Vec<Filter>) {
        self.inner.save_subscription(id, filters).await
    }

    /// Send a client message to specific relays
    ///
    /// Note: **the relays must already be added!**
    pub async fn send_msg_to<I, U>(&self, urls: I, msg: ClientMessage) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        self.batch_msg_to(urls, vec![msg]).await
    }

    /// Send multiple client messages at once to specific relays
    ///
    /// Note: **the relays must already be added!**
    pub async fn batch_msg_to<I, U>(
        &self,
        urls: I,
        msgs: Vec<ClientMessage>,
    ) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Compose URLs
        let set: HashSet<RelayUrl> = urls
            .into_iter()
            .map(|u| u.try_into_url())
            .collect::<Result<_, _>>()?;

        // Check if urls set is empty
        if set.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !set.iter().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        // Save events
        for msg in msgs.iter() {
            if let ClientMessage::Event(event) = msg {
                self.inner.state.database().save_event(event).await?;
            }
        }

        let mut output: Output<()> = Output::default();

        // Batch messages and construct outputs
        for url in set.into_iter() {
            let relay: &Relay = self.inner.internal_relay(&relays, &url)?;
            match relay.batch_msg(msgs.clone()) {
                Ok(..) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        if output.success.is_empty() {
            return Err(Error::Failed);
        }

        Ok(output)
    }

    /// Send event to all relays with `WRITE` flag (check [`RelayServiceFlags`] for more details).
    pub async fn send_event(&self, event: Event) -> Result<Output<EventId>, Error> {
        let urls: Vec<RelayUrl> = self.inner.write_relay_urls().await;
        self.send_event_to(urls, event).await
    }

    /// Send multiple events at once to all relays with `WRITE` flag (check [`RelayServiceFlags`] for more details).
    #[deprecated(since = "0.38.0")]
    pub async fn batch_event(&self, _events: Vec<Event>) -> Result<Output<()>, Error> {
        unimplemented!()
    }

    /// Send event to specific relays
    pub async fn send_event_to<I, U>(&self, urls: I, event: Event) -> Result<Output<EventId>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Compose URLs
        let set: HashSet<RelayUrl> = urls
            .into_iter()
            .map(|u| u.try_into_url())
            .collect::<Result<_, _>>()?;

        // Check if urls set is empty
        if set.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !set.iter().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        // Save event into database
        self.inner.state.database().save_event(&event).await?;

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(set.len());
        let mut futures = Vec::with_capacity(set.len());
        let mut output: Output<EventId> = Output {
            val: event.id,
            success: HashSet::new(),
            failed: HashMap::new(),
        };

        // Compose futures
        for url in set.into_iter() {
            let relay: &Relay = self.inner.internal_relay(&relays, &url)?;
            let event: Event = event.clone();
            urls.push(url);
            futures.push(relay.send_event(event));
        }

        // Join futures
        let list = future::join_all(futures).await;

        // Iter results and construct output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(..) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        if output.success.is_empty() {
            return Err(Error::Failed);
        }

        Ok(output)
    }

    /// Send multiple events at once to specific relays
    #[deprecated(since = "0.38.0")]
    pub async fn batch_event_to<I, U>(
        &self,
        _urls: I,
        _events: Vec<Event>,
    ) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        unimplemented!()
    }

    /// Subscribe to filters to all relays with `READ` flag.
    ///
    /// Check [`RelayPool::subscribe_with_id_to`] docs to learn more.
    pub async fn subscribe(
        &self,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<SubscriptionId>, Error> {
        let id: SubscriptionId = SubscriptionId::generate();
        let output: Output<()> = self.subscribe_with_id(id.clone(), filters, opts).await?;
        Ok(Output {
            val: id,
            success: output.success,
            failed: output.failed,
        })
    }

    /// Subscribe to filters with custom [SubscriptionId] to all relays with `READ` flag.
    ///
    /// Check [`RelayPool::subscribe_with_id_to`] docs to learn more.
    pub async fn subscribe_with_id(
        &self,
        id: SubscriptionId,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<()>, Error> {
        // Check if isn't auto-closing subscription
        if !opts.is_auto_closing() {
            // Save subscription
            self.save_subscription(id.clone(), filters.clone()).await;
        }

        // Get relay urls
        let urls: Vec<RelayUrl> = self.inner.read_relay_urls().await;

        // Subscribe
        self.subscribe_with_id_to(urls, id, filters, opts).await
    }

    /// Subscribe to filters to specific relays
    ///
    /// Check [`RelayPool::subscribe_with_id_to`] docs to learn more.
    pub async fn subscribe_to<I, U>(
        &self,
        urls: I,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<SubscriptionId>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let id: SubscriptionId = SubscriptionId::generate();
        let output: Output<()> = self
            .subscribe_with_id_to(urls, id.clone(), filters, opts)
            .await?;
        Ok(Output {
            val: id,
            success: output.success,
            failed: output.failed,
        })
    }

    /// Subscribe to filters with custom [SubscriptionId] to specific relays
    ///
    /// This method doesn't add relays!
    /// All the relays must be added to the pool with [`RelayPool::add_relay`].
    /// If the specified relays don't exist, [`Error::RelayNotFound`] is returned.
    ///
    /// ### Auto-closing subscription
    ///
    /// It's possible to automatically close a subscription by configuring the [SubscribeOptions].
    ///
    /// Auto-closing subscriptions aren't saved in the subscription map!
    pub async fn subscribe_with_id_to<I, U>(
        &self,
        urls: I,
        id: SubscriptionId,
        filters: Vec<Filter>,
        opts: SubscribeOptions,
    ) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let targets = urls.into_iter().map(|u| (u, filters.clone()));
        self.subscribe_targeted(id, targets, opts).await
    }

    /// Targeted subscription
    ///
    /// Subscribe to specific relays with specific filters.
    pub async fn subscribe_targeted<I, U>(
        &self,
        id: SubscriptionId,
        targets: I,
        opts: SubscribeOptions,
    ) -> Result<Output<()>, Error>
    where
        I: IntoIterator<Item = (U, Vec<Filter>)>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Collect targets
        let targets: HashMap<RelayUrl, Vec<Filter>> = targets
            .into_iter()
            .map(|(u, f)| Ok((u.try_into_url()?, f)))
            .collect::<Result<_, Error>>()?;

        // Check if urls set is empty
        if targets.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Check if relays map is empty
        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !targets.keys().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(targets.len());
        let mut futures = Vec::with_capacity(targets.len());
        let mut output: Output<()> = Output::default();

        // Compose futures
        for (url, filters) in targets.into_iter() {
            let relay: &Relay = self.inner.internal_relay(&relays, &url)?;
            let id: SubscriptionId = id.clone();
            urls.push(url);
            futures.push(relay.subscribe_with_id(id, filters, opts));
        }

        // Join futures
        let list = future::join_all(futures).await;

        // Iter results and construct output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(..) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        if output.success.is_empty() {
            return Err(Error::Failed);
        }

        Ok(output)
    }

    /// Unsubscribe from subscription
    pub async fn unsubscribe(&self, id: SubscriptionId) {
        // Remove subscription from pool
        self.inner.remove_subscription(&id).await;

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // TODO: use join_all and return `Output`?

        // Remove subscription from relays
        for relay in relays.values() {
            if let Err(e) = relay.unsubscribe(id.clone()).await {
                tracing::error!("{e}");
            }
        }
    }

    /// Unsubscribe from all subscriptions
    pub async fn unsubscribe_all(&self) {
        // Remove subscriptions from pool
        self.inner.remove_all_subscriptions().await;

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // TODO: use join_all and return `Output`?

        // Unsubscribe relays
        for relay in relays.values() {
            if let Err(e) = relay.unsubscribe_all().await {
                tracing::error!("{e}");
            }
        }
    }

    /// Sync events with relays (negentropy reconciliation)
    pub async fn sync(
        &self,
        filter: Filter,
        opts: &SyncOptions,
    ) -> Result<Output<Reconciliation>, Error> {
        let urls: Vec<RelayUrl> = self.inner.relay_urls().await;
        self.sync_with(urls, filter, opts).await
    }

    /// Sync events with specific relays (negentropy reconciliation)
    pub async fn sync_with<I, U>(
        &self,
        urls: I,
        filter: Filter,
        opts: &SyncOptions,
    ) -> Result<Output<Reconciliation>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Get items
        let items: Vec<(EventId, Timestamp)> = self
            .inner
            .state
            .database()
            .negentropy_items(filter.clone())
            .await?;

        // Compose filters
        let mut filters: HashMap<Filter, Vec<(EventId, Timestamp)>> = HashMap::with_capacity(1);
        filters.insert(filter, items);

        // Reconcile
        let targets = urls.into_iter().map(|u| (u, filters.clone()));
        self.sync_targeted(targets, opts).await
    }

    /// Sync events with specific relays and filters (negentropy reconciliation)
    pub async fn sync_targeted<I, U>(
        &self,
        targets: I,
        opts: &SyncOptions,
    ) -> Result<Output<Reconciliation>, Error>
    where
        I: IntoIterator<Item = (U, HashMap<Filter, Vec<(EventId, Timestamp)>>)>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        // Collect targets
        let targets: HashMap<RelayUrl, HashMap<Filter, Vec<(EventId, Timestamp)>>> = targets
            .into_iter()
            .map(|(u, v)| Ok((u.try_into_url()?, v)))
            .collect::<Result<_, Error>>()?;

        // Check if urls set is empty
        if targets.is_empty() {
            return Err(Error::NoRelaysSpecified);
        }

        // Lock with read shared access
        let relays = self.inner.atomic.relays.read().await;

        // Check if empty
        if relays.is_empty() {
            return Err(Error::NoRelays);
        }

        // Check if urls set contains ONLY already added relays
        if !targets.keys().all(|url| relays.contains_key(url)) {
            return Err(Error::RelayNotFound);
        }

        // TODO: shared reconciliation output to avoid to request duplicates?

        let mut urls: Vec<RelayUrl> = Vec::with_capacity(targets.len());
        let mut futures = Vec::with_capacity(targets.len());
        let mut output: Output<Reconciliation> = Output::default();

        // Compose futures
        for (url, filters) in targets.into_iter() {
            let relay: &Relay = self.inner.internal_relay(&relays, &url)?;
            urls.push(url);
            futures.push(relay.sync_multi(filters, opts));
        }

        // Join futures
        let list = future::join_all(futures).await;

        // Iter results and constructs output
        for (url, result) in urls.into_iter().zip(list.into_iter()) {
            match result {
                Ok(reconciliation) => {
                    // Success, insert relay url in 'success' set result
                    output.success.insert(url);
                    output.merge(reconciliation);
                }
                Err(e) => {
                    output.failed.insert(url, e.to_string());
                }
            }
        }

        // Check if sync failed (no success)
        if output.success.is_empty() {
            return Err(Error::NegentropyReconciliationFailed);
        }

        Ok(output)
    }

    /// Fetch events from relays with [`RelayServiceFlags::READ`] flag.
    pub async fn fetch_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<Events, Error> {
        let urls: Vec<RelayUrl> = self.inner.read_relay_urls().await;
        self.fetch_events_from(urls, filters, timeout, policy).await
    }

    /// Fetch events from specific relays
    pub async fn fetch_events_from<I, U>(
        &self,
        urls: I,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<Events, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let mut events: Events = Events::new(&filters);

        // Stream events
        let mut stream = self
            .stream_events_from(urls, filters, timeout, policy)
            .await?;
        while let Some(event) = stream.next().await {
            events.insert(event);
        }

        Ok(events)
    }

    /// Stream events from relays with `READ` flag.
    pub async fn stream_events(
        &self,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<ReceiverStream<Event>, Error> {
        let urls: Vec<RelayUrl> = self.inner.read_relay_urls().await;
        self.stream_events_from(urls, filters, timeout, policy)
            .await
    }

    /// Stream events from specific relays
    pub async fn stream_events_from<I, U>(
        &self,
        urls: I,
        filters: Vec<Filter>,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<ReceiverStream<Event>, Error>
    where
        I: IntoIterator<Item = U>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        let targets = urls.into_iter().map(|u| (u, filters.clone()));
        self.stream_events_targeted(targets, timeout, policy).await
    }

    /// Targeted streaming events
    ///
    /// Stream events from specific relays with specific filters
    pub async fn stream_events_targeted<I, U>(
        &self,
        targets: I,
        timeout: Duration,
        policy: ReqExitPolicy,
    ) -> Result<ReceiverStream<Event>, Error>
    where
        I: IntoIterator<Item = (U, Vec<Filter>)>,
        U: TryIntoUrl,
        Error: From<<U as TryIntoUrl>::Err>,
    {
        self.inner
            .stream_events_targeted(targets, timeout, policy)
            .await
    }

    /// Handle notifications
    pub async fn handle_notifications<F, Fut>(&self, func: F) -> Result<(), Error>
    where
        F: Fn(RelayPoolNotification) -> Fut,
        Fut: Future<Output = Result<bool>>,
    {
        let mut notifications = self.notifications();
        while let Ok(notification) = notifications.recv().await {
            let shutdown: bool = RelayPoolNotification::Shutdown == notification;
            let exit: bool = func(notification)
                .await
                .map_err(|e| Error::Handler(e.to_string()))?;
            if exit || shutdown {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nostr_relay_builder::MockRelay;

    use super::*;

    #[tokio::test]
    async fn test_shutdown() {
        let mock = MockRelay::run().await.unwrap();
        let url = mock.url();

        let pool = RelayPool::default();

        pool.add_relay(&url, RelayOptions::default()).await.unwrap();

        pool.connect().await;

        assert!(!pool.inner.is_shutdown());

        tokio::time::sleep(Duration::from_secs(1)).await;

        pool.shutdown().await.unwrap();

        assert!(pool.inner.is_shutdown());

        assert!(matches!(
            pool.add_relay(&url, RelayOptions::default())
                .await
                .unwrap_err(),
            Error::Shutdown
        ));
    }
}
