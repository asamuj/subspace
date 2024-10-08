//! A container that caches pieces
//!
//! Farmer cache is a container that orchestrates a bunch of piece and plot caches that together
//! persist pieces in a way that is easy to retrieve comparing to decoding pieces from plots.

mod metrics;
mod piece_cache_state;
#[cfg(test)]
mod tests;

use crate::farm::{MaybePieceStoredResult, PieceCache, PieceCacheId, PieceCacheOffset, PlotCache};
use crate::farmer_cache::metrics::FarmerCacheMetrics;
use crate::farmer_cache::piece_cache_state::PieceCachesState;
use crate::node_client::NodeClient;
use crate::utils::run_future_in_dedicated_thread;
use async_lock::RwLock as AsyncRwLock;
use event_listener_primitives::{Bag, HandlerId};
use futures::stream::{FuturesOrdered, FuturesUnordered};
use futures::{select, FutureExt, StreamExt};
use prometheus_client::registry::Registry;
use rayon::prelude::*;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{fmt, mem};
use subspace_core_primitives::{Piece, PieceIndex, SegmentHeader, SegmentIndex};
use subspace_farmer_components::PieceGetter;
use subspace_networking::libp2p::kad::{ProviderRecord, RecordKey};
use subspace_networking::libp2p::PeerId;
use subspace_networking::utils::multihash::ToMultihash;
use subspace_networking::{KeyWrapper, LocalRecordProvider, UniqueRecordBinaryHeap};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::{block_in_place, yield_now};
use tracing::{debug, error, info, trace, warn};

const WORKER_CHANNEL_CAPACITY: usize = 100;
const CONCURRENT_PIECES_TO_DOWNLOAD: usize = 1_000;
/// Make caches available as they are building without waiting for the initialization to finish,
/// this number defines an interval in pieces after which cache is updated
const INTERMEDIATE_CACHE_UPDATE_INTERVAL: usize = 100;
const INITIAL_SYNC_FARM_INFO_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// How long to wait for `is_piece_maybe_stored` response from plot cache before timing out in order
/// to prevent blocking of executor for too long
const IS_PIECE_MAYBE_STORED_TIMEOUT: Duration = Duration::from_millis(100);

type HandlerFn<A> = Arc<dyn Fn(&A) + Send + Sync + 'static>;
type Handler<A> = Bag<HandlerFn<A>, A>;

#[derive(Default, Debug)]
struct Handlers {
    progress: Handler<f32>,
}

#[derive(Debug, Clone, Copy)]
struct FarmerCacheOffset<CacheIndex> {
    cache_index: CacheIndex,
    piece_offset: PieceCacheOffset,
}

impl<CacheIndex> FarmerCacheOffset<CacheIndex>
where
    CacheIndex: Hash + Eq + Copy + fmt::Debug + fmt::Display + Send + Sync + 'static,
    CacheIndex: TryFrom<usize>,
{
    fn new(cache_index: CacheIndex, piece_offset: PieceCacheOffset) -> Self {
        Self {
            cache_index,
            piece_offset,
        }
    }
}

#[derive(Debug, Clone)]
struct CacheBackend {
    backend: Arc<dyn PieceCache>,
    used_capacity: u32,
    total_capacity: u32,
}

impl std::ops::Deref for CacheBackend {
    type Target = Arc<dyn PieceCache>;

    fn deref(&self) -> &Self::Target {
        &self.backend
    }
}

impl CacheBackend {
    fn new(backend: Arc<dyn PieceCache>, total_capacity: u32) -> Self {
        Self {
            backend,
            used_capacity: 0,
            total_capacity,
        }
    }

    fn next_free(&mut self) -> Option<PieceCacheOffset> {
        let offset = self.used_capacity;
        if offset < self.total_capacity {
            self.used_capacity += 1;
            Some(PieceCacheOffset(offset))
        } else {
            debug!(?offset, total_capacity = ?self.total_capacity, "No free space in cache backend");
            None
        }
    }

    fn free_size(&self) -> u32 {
        self.total_capacity - self.used_capacity
    }
}

#[derive(Debug)]
struct CacheState<CacheIndex> {
    cache_stored_pieces: HashMap<RecordKey, FarmerCacheOffset<CacheIndex>>,
    cache_free_offsets: Vec<FarmerCacheOffset<CacheIndex>>,
    backend: CacheBackend,
}

#[derive(Debug)]
enum WorkerCommand {
    ReplaceBackingCaches {
        new_piece_caches: Vec<Arc<dyn PieceCache>>,
    },
    ForgetKey {
        key: RecordKey,
    },
}

#[derive(Debug)]
struct CacheWorkerState {
    heap: UniqueRecordBinaryHeap<KeyWrapper<PieceIndex>>,
    last_segment_index: SegmentIndex,
}

/// Farmer cache worker used to drive the farmer cache backend
#[derive(Debug)]
#[must_use = "Farmer cache will not work unless its worker is running"]
pub struct FarmerCacheWorker<NC, CacheIndex>
where
    NC: fmt::Debug,
{
    peer_id: PeerId,
    node_client: NC,
    piece_caches: Arc<AsyncRwLock<PieceCachesState<CacheIndex>>>,
    plot_caches: Arc<PlotCaches>,
    handlers: Arc<Handlers>,
    worker_receiver: Option<mpsc::Receiver<WorkerCommand>>,
    metrics: Option<Arc<FarmerCacheMetrics>>,
}

impl<NC, CacheIndex> FarmerCacheWorker<NC, CacheIndex>
where
    NC: NodeClient,
    CacheIndex: Hash + Eq + Copy + fmt::Debug + fmt::Display + Send + Sync + 'static,
    usize: From<CacheIndex>,
    CacheIndex: TryFrom<usize>,
{
    /// Run the cache worker with provided piece getter.
    ///
    /// NOTE: Piece getter must not depend on farmer cache in order to avoid reference cycles!
    pub async fn run<PG>(mut self, piece_getter: PG)
    where
        PG: PieceGetter,
    {
        // Limit is dynamically set later
        let mut worker_state = CacheWorkerState {
            heap: UniqueRecordBinaryHeap::new(self.peer_id, 0),
            last_segment_index: SegmentIndex::ZERO,
        };

        let mut worker_receiver = self
            .worker_receiver
            .take()
            .expect("Always set during worker instantiation");

        if let Some(WorkerCommand::ReplaceBackingCaches { new_piece_caches }) =
            worker_receiver.recv().await
        {
            self.initialize(&piece_getter, &mut worker_state, new_piece_caches)
                .await;
        } else {
            // Piece cache is dropped before backing caches were sent
            return;
        }

        let mut segment_headers_notifications =
            match self.node_client.subscribe_archived_segment_headers().await {
                Ok(segment_headers_notifications) => segment_headers_notifications,
                Err(error) => {
                    error!(%error, "Failed to subscribe to archived segments notifications");
                    return;
                }
            };

        // Keep up with segment indices that were potentially created since reinitialization,
        // depending on the size of the diff this may pause block production for a while (due to
        // subscription we have created above)
        self.keep_up_after_initial_sync(&piece_getter, &mut worker_state)
            .await;

        loop {
            select! {
                maybe_command = worker_receiver.recv().fuse() => {
                    let Some(command) = maybe_command else {
                        // Nothing else left to do
                        return;
                    };

                    self.handle_command(command, &piece_getter, &mut worker_state).await;
                }
                maybe_segment_header = segment_headers_notifications.next().fuse() => {
                    if let Some(segment_header) = maybe_segment_header {
                        self.process_segment_header(segment_header, &mut worker_state).await;
                    } else {
                        // Keep-up sync only ends with subscription, which lasts for duration of an
                        // instance
                        return;
                    }
                }
            }
        }
    }

    async fn handle_command<PG>(
        &self,
        command: WorkerCommand,
        piece_getter: &PG,
        worker_state: &mut CacheWorkerState,
    ) where
        PG: PieceGetter,
    {
        match command {
            WorkerCommand::ReplaceBackingCaches { new_piece_caches } => {
                self.initialize(piece_getter, worker_state, new_piece_caches)
                    .await;
            }
            // TODO: Consider implementing optional re-sync of the piece instead of just forgetting
            WorkerCommand::ForgetKey { key } => {
                let mut caches = self.piece_caches.write().await;
                let Some(offset) = caches.remove_stored_piece(&key) else {
                    // Key not exist
                    return;
                };

                let cache_index = offset.cache_index;
                let piece_offset = offset.piece_offset;
                let Some(backend) = caches.get_backend(cache_index).cloned() else {
                    // Cache backend not exist
                    return;
                };

                caches.push_dangling_free_offset(offset);
                match backend.read_piece_index(piece_offset).await {
                    Ok(Some(piece_index)) => {
                        worker_state.heap.remove(KeyWrapper(piece_index));
                    }
                    Ok(None) => {
                        warn!(
                            %cache_index,
                            %piece_offset,
                            "Piece index out of range, this is likely an implementation bug, \
                            not freeing heap element"
                        );
                    }
                    Err(error) => {
                        error!(
                            %error,
                            %cache_index,
                            ?key,
                            %piece_offset,
                            "Error while reading piece from cache, might be a disk corruption"
                        );
                    }
                }
            }
        }
    }

    async fn initialize<PG>(
        &self,
        piece_getter: &PG,
        worker_state: &mut CacheWorkerState,
        new_piece_caches: Vec<Arc<dyn PieceCache>>,
    ) where
        PG: PieceGetter,
    {
        info!("Initializing piece cache");

        // Pull old cache state since it will be replaced with a new one and reuse its allocations
        let (mut stored_pieces, mut dangling_free_offsets) =
            mem::take(&mut *self.piece_caches.write().await).reuse();

        debug!("Collecting pieces that were in the cache before");

        if let Some(metrics) = &self.metrics {
            metrics.piece_cache_capacity_total.set(0);
            metrics.piece_cache_capacity_used.set(0);
        }

        // Build cache state of all backends
        let piece_caches_number = new_piece_caches.len();
        let maybe_caches_futures = new_piece_caches
            .into_iter()
            .enumerate()
            .filter_map(|(cache_index, new_cache)| {
                let total_capacity = new_cache.max_num_elements();
                let mut backend = CacheBackend::new(new_cache, total_capacity);
                let Ok(cache_index) = CacheIndex::try_from(cache_index) else {
                    warn!(
                        ?piece_caches_number,
                        "Too many piece caches provided, {cache_index} cache will be ignored",
                    );
                    return None;
                };

                if let Some(metrics) = &self.metrics {
                    metrics
                        .piece_cache_capacity_total
                        .inc_by(total_capacity as i64);
                }

                let init_fut = async move {
                    let used_capacity = &mut backend.used_capacity;

                    // Hack with first collecting into `Option` with `Option::take()` call
                    // later is to satisfy compiler that gets confused about ownership
                    // otherwise
                    let mut maybe_contents = match backend.backend.contents().await {
                        Ok(contents) => Some(contents),
                        Err(error) => {
                            warn!(%cache_index, %error, "Failed to get cache contents");

                            None
                        }
                    };

                    #[allow(clippy::mutable_key_type)]
                    let mut cache_stored_pieces = HashMap::new();
                    let mut cache_free_offsets = Vec::new();

                    let Some(mut contents) = maybe_contents.take() else {
                        drop(maybe_contents);

                        return CacheState {
                            cache_stored_pieces,
                            cache_free_offsets,
                            backend,
                        };
                    };

                    while let Some(maybe_element_details) = contents.next().await {
                        let (piece_offset, maybe_piece_index) = match maybe_element_details {
                            Ok(element_details) => element_details,
                            Err(error) => {
                                warn!(
                                    %cache_index,
                                    %error,
                                    "Failed to get cache contents element details"
                                );
                                break;
                            }
                        };
                        let offset = FarmerCacheOffset::new(cache_index, piece_offset);
                        match maybe_piece_index {
                            Some(piece_index) => {
                                *used_capacity = piece_offset.0 + 1;
                                cache_stored_pieces
                                    .insert(RecordKey::from(piece_index.to_multihash()), offset);
                            }
                            None => {
                                // TODO: Optimize to not store all free offsets, only dangling
                                //  offsets are actually necessary
                                cache_free_offsets.push(offset);
                            }
                        }

                        // Allow for task to be aborted
                        yield_now().await;
                    }

                    drop(maybe_contents);
                    drop(contents);

                    CacheState {
                        cache_stored_pieces,
                        cache_free_offsets,
                        backend,
                    }
                };

                Some(run_future_in_dedicated_thread(
                    move || init_fut,
                    format!("piece-cache.{cache_index}"),
                ))
            })
            .collect::<Result<Vec<_>, _>>();

        let caches_futures = match maybe_caches_futures {
            Ok(caches_futures) => caches_futures,
            Err(error) => {
                error!(%error, "Failed to spawn piece cache reading thread");

                return;
            }
        };

        let mut backends = Vec::with_capacity(caches_futures.len());
        let mut caches_futures = caches_futures.into_iter().collect::<FuturesOrdered<_>>();

        while let Some(maybe_cache) = caches_futures.next().await {
            match maybe_cache {
                Ok(cache) => {
                    let backend = cache.backend;
                    stored_pieces.extend(cache.cache_stored_pieces.into_iter());
                    dangling_free_offsets.extend(
                        cache.cache_free_offsets.into_iter().filter(|free_offset| {
                            free_offset.piece_offset.0 < backend.used_capacity
                        }),
                    );
                    backends.push(backend);
                }
                Err(_cancelled) => {
                    error!("Piece cache reading thread panicked");

                    return;
                }
            };
        }

        let mut caches = PieceCachesState::new(stored_pieces, dangling_free_offsets, backends);

        info!("Synchronizing piece cache");

        let last_segment_index = loop {
            match self.node_client.farmer_app_info().await {
                Ok(farmer_app_info) => {
                    let last_segment_index =
                        farmer_app_info.protocol_info.history_size.segment_index();
                    // Wait for node to be either fully synced or to be aware of non-zero segment
                    // index, which would indicate it has started DSN sync and knows about
                    // up-to-date archived history.
                    //
                    // While this doesn't account for situations where node was offline for a long
                    // time and is aware of old segment headers, this is good enough for piece cache
                    // sync to proceed and should result in better user experience on average.
                    if !farmer_app_info.syncing || last_segment_index > SegmentIndex::ZERO {
                        break last_segment_index;
                    }
                }
                Err(error) => {
                    error!(
                        %error,
                        "Failed to get farmer app info from node, keeping old cache state without \
                        updates"
                    );

                    // Not the latest, but at least something
                    *self.piece_caches.write().await = caches;
                    return;
                }
            }

            tokio::time::sleep(INITIAL_SYNC_FARM_INFO_CHECK_INTERVAL).await;
        };

        debug!(%last_segment_index, "Identified last segment index");

        let limit = caches.total_capacity();
        worker_state.heap.clear();
        // Change limit to number of pieces
        worker_state.heap.set_limit(limit);

        for segment_index in SegmentIndex::ZERO..=last_segment_index {
            for piece_index in segment_index.segment_piece_indexes() {
                worker_state.heap.insert(KeyWrapper(piece_index));
            }
        }

        // This hashset is faster than `heap`
        // Clippy complains about `RecordKey`, but it is not changing here, so it is fine
        #[allow(clippy::mutable_key_type)]
        let mut piece_indices_to_store = worker_state
            .heap
            .keys()
            .map(|KeyWrapper(piece_index)| {
                (RecordKey::from(piece_index.to_multihash()), *piece_index)
            })
            .collect::<HashMap<_, _>>();

        let mut piece_caches_capacity_used = vec![0u32; caches.backends().len()];
        // Filter-out piece indices that are stored, but should not be as well as clean
        // `inserted_piece_indices` from already stored piece indices, leaving just those that are
        // still missing in cache
        caches.free_unneeded_stored_pieces(&mut piece_indices_to_store);

        if let Some(metrics) = &self.metrics {
            for offset in caches.stored_pieces_offests() {
                piece_caches_capacity_used[usize::from(offset.cache_index)] += 1;
            }

            for cache_used in piece_caches_capacity_used {
                metrics
                    .piece_cache_capacity_used
                    .inc_by(i64::from(cache_used));
            }
        }

        // Store whatever correct pieces are immediately available after restart
        self.piece_caches.write().await.clone_from(&caches);

        debug!(
            count = %piece_indices_to_store.len(),
            "Identified piece indices that should be cached",
        );

        let mut piece_indices_to_store = piece_indices_to_store.into_values().collect::<Vec<_>>();
        // Sort pieces such that they are in ascending order and have higher chance of download
        // overlapping with other processes like node's sync from DSN
        piece_indices_to_store.par_sort_unstable();
        let mut piece_indices_to_store = piece_indices_to_store.into_iter();

        let download_piece = |piece_index| async move {
            trace!(%piece_index, "Downloading piece");

            let result = piece_getter.get_piece(piece_index).await;

            match result {
                Ok(Some(piece)) => {
                    trace!(%piece_index, "Downloaded piece successfully");

                    Some((piece_index, piece))
                }
                Ok(None) => {
                    debug!(%piece_index, "Couldn't find piece");
                    None
                }
                Err(error) => {
                    debug!(%error, %piece_index, "Failed to get piece for piece cache");
                    None
                }
            }
        };

        let pieces_to_download_total = piece_indices_to_store.len();
        let mut downloading_pieces = piece_indices_to_store
            .by_ref()
            .take(CONCURRENT_PIECES_TO_DOWNLOAD)
            .map(download_piece)
            .collect::<FuturesUnordered<_>>();

        let mut downloaded_pieces_count = 0;
        self.handlers.progress.call_simple(&0.0);
        while let Some(maybe_piece) = downloading_pieces.next().await {
            // Push another piece to download
            if let Some(piece_index_to_download) = piece_indices_to_store.next() {
                downloading_pieces.push(download_piece(piece_index_to_download));
            }

            let Some((piece_index, piece)) = &maybe_piece else {
                continue;
            };

            // Find plot in which there is a place for new piece to be stored
            let Some(offset) = caches.pop_free_offset() else {
                error!(
                    %piece_index,
                    "Failed to store piece in cache, there was no space"
                );
                break;
            };

            let cache_index = offset.cache_index;
            let piece_offset = offset.piece_offset;
            if let Some(backend) = caches.get_backend(cache_index)
                && let Err(error) = backend.write_piece(piece_offset, *piece_index, piece).await
            {
                // TODO: Will likely need to cache problematic backend indices to avoid hitting it over and over again repeatedly
                error!(
                    %error,
                    %cache_index,
                    %piece_index,
                    %piece_offset,
                    "Failed to write piece into cache"
                );
                continue;
            }
            caches.push_stored_piece(RecordKey::from(piece_index.to_multihash()), offset);

            downloaded_pieces_count += 1;
            // Do not print anything or send progress notification after last piece until piece
            // cache is written fully below
            if downloaded_pieces_count != pieces_to_download_total {
                let progress =
                    downloaded_pieces_count as f32 / pieces_to_download_total as f32 * 100.0;
                if downloaded_pieces_count % INTERMEDIATE_CACHE_UPDATE_INTERVAL == 0 {
                    self.piece_caches.write().await.clone_from(&caches);

                    info!("Piece cache sync {progress:.2}% complete");
                }

                self.handlers.progress.call_simple(&progress);
            }
        }

        *self.piece_caches.write().await = caches;
        self.handlers.progress.call_simple(&100.0);
        worker_state.last_segment_index = last_segment_index;

        info!("Finished piece cache synchronization");
    }

    async fn process_segment_header(
        &self,
        segment_header: SegmentHeader,
        worker_state: &mut CacheWorkerState,
    ) {
        let segment_index = segment_header.segment_index();
        debug!(%segment_index, "Starting to process newly archived segment");

        if worker_state.last_segment_index < segment_index {
            debug!(%segment_index, "Downloading potentially useful pieces");

            // We do not insert pieces into cache/heap yet, so we don't know if all of these pieces
            // will be included, but there is a good chance they will be, and we want to acknowledge
            // new segment header as soon as possible
            let pieces_to_maybe_include = segment_index
                .segment_piece_indexes()
                .into_iter()
                .map(|piece_index| {
                    let worker_state = &*worker_state;

                    async move {
                        let should_store_in_piece_cache = worker_state
                            .heap
                            .should_include_key(KeyWrapper(piece_index));
                        let key = RecordKey::from(piece_index.to_multihash());
                        let should_store_in_plot_cache =
                            self.plot_caches.should_store(piece_index, &key).await;

                        if !(should_store_in_piece_cache || should_store_in_plot_cache) {
                            trace!(%piece_index, "Piece doesn't need to be cached #1");

                            return None;
                        }

                        let maybe_piece = match self.node_client.piece(piece_index).await {
                            Ok(maybe_piece) => maybe_piece,
                            Err(error) => {
                                error!(
                                    %error,
                                    %segment_index,
                                    %piece_index,
                                    "Failed to retrieve piece from node right after archiving, \
                                    this should never happen and is an implementation bug"
                                );

                                return None;
                            }
                        };

                        let Some(piece) = maybe_piece else {
                            error!(
                                %segment_index,
                                %piece_index,
                                "Failed to retrieve piece from node right after archiving, this \
                                should never happen and is an implementation bug"
                            );

                            return None;
                        };

                        Some((piece_index, piece))
                    }
                })
                .collect::<FuturesUnordered<_>>()
                .filter_map(|maybe_piece| async move { maybe_piece })
                .collect::<Vec<_>>()
                .await;

            debug!(%segment_index, "Downloaded potentially useful pieces");

            self.acknowledge_archived_segment_processing(segment_index)
                .await;

            // TODO: Would be nice to have concurrency here, but heap is causing a bit of
            //  difficulties unfortunately
            // Go through potentially matching pieces again now that segment was acknowledged and
            // try to persist them if necessary
            for (piece_index, piece) in pieces_to_maybe_include {
                if !self
                    .plot_caches
                    .store_additional_piece(piece_index, &piece)
                    .await
                {
                    trace!(%piece_index, "Piece doesn't need to be cached in plot cache");
                }

                if !worker_state
                    .heap
                    .should_include_key(KeyWrapper(piece_index))
                {
                    trace!(%piece_index, "Piece doesn't need to be cached #2");

                    continue;
                }

                trace!(%piece_index, "Piece needs to be cached #1");

                self.persist_piece_in_cache(piece_index, piece, worker_state)
                    .await;
            }

            worker_state.last_segment_index = segment_index;
        } else {
            self.acknowledge_archived_segment_processing(segment_index)
                .await;
        }

        debug!(%segment_index, "Finished processing newly archived segment");
    }

    async fn acknowledge_archived_segment_processing(&self, segment_index: SegmentIndex) {
        match self
            .node_client
            .acknowledge_archived_segment_header(segment_index)
            .await
        {
            Ok(()) => {
                debug!(%segment_index, "Acknowledged archived segment");
            }
            Err(error) => {
                error!(%segment_index, ?error, "Failed to acknowledge archived segment");
            }
        };
    }

    async fn keep_up_after_initial_sync<PG>(
        &self,
        piece_getter: &PG,
        worker_state: &mut CacheWorkerState,
    ) where
        PG: PieceGetter,
    {
        let last_segment_index = match self.node_client.farmer_app_info().await {
            Ok(farmer_app_info) => farmer_app_info.protocol_info.history_size.segment_index(),
            Err(error) => {
                error!(
                    %error,
                    "Failed to get farmer app info from node, keeping old cache state without \
                    updates"
                );
                return;
            }
        };

        if last_segment_index <= worker_state.last_segment_index {
            return;
        }

        info!(
            "Syncing piece cache to the latest history size, this may pause block production if \
            takes too long"
        );

        // Keep up with segment indices that were potentially created since reinitialization
        let piece_indices = (worker_state.last_segment_index..=last_segment_index)
            .flat_map(|segment_index| segment_index.segment_piece_indexes());

        // TODO: Can probably do concurrency here
        for piece_index in piece_indices {
            let key = KeyWrapper(piece_index);
            if !worker_state.heap.should_include_key(key) {
                trace!(%piece_index, "Piece doesn't need to be cached #3");

                continue;
            }

            trace!(%piece_index, "Piece needs to be cached #2");

            let result = piece_getter.get_piece(piece_index).await;

            let piece = match result {
                Ok(Some(piece)) => piece,
                Ok(None) => {
                    debug!(%piece_index, "Couldn't find piece");
                    continue;
                }
                Err(error) => {
                    debug!(
                        %error,
                        %piece_index,
                        "Failed to get piece for piece cache"
                    );
                    continue;
                }
            };

            self.persist_piece_in_cache(piece_index, piece, worker_state)
                .await;
        }

        info!("Finished syncing piece cache to the latest history size");

        worker_state.last_segment_index = last_segment_index;
    }

    /// This assumes it was already checked that piece needs to be stored, no verification for this
    /// is done internally and invariants will break if this assumption doesn't hold true
    async fn persist_piece_in_cache(
        &self,
        piece_index: PieceIndex,
        piece: Piece,
        worker_state: &mut CacheWorkerState,
    ) {
        let record_key = RecordKey::from(piece_index.to_multihash());
        let heap_key = KeyWrapper(piece_index);

        let mut caches = self.piece_caches.write().await;
        match worker_state.heap.insert(heap_key) {
            // Entry is already occupied, we need to find and replace old piece with new one
            Some(KeyWrapper(old_piece_index)) => {
                let old_record_key = RecordKey::from(old_piece_index.to_multihash());
                let Some(offset) = caches.remove_stored_piece(&old_record_key) else {
                    // Not this disk farm
                    warn!(
                        %old_piece_index,
                        %piece_index,
                        "Should have replaced cached piece, but it didn't happen, this is an \
                        implementation bug"
                    );
                    return;
                };

                let cache_index = offset.cache_index;
                let piece_offset = offset.piece_offset;
                let Some(backend) = caches.get_backend(cache_index) else {
                    // Cache backend not exist
                    warn!(
                        %cache_index,
                        %piece_index,
                        "Should have a cached backend, but it didn't exist, this is an \
                        implementation bug"
                    );
                    return;
                };
                if let Err(error) = backend.write_piece(piece_offset, piece_index, &piece).await {
                    error!(
                        %error,
                        %cache_index,
                        %piece_index,
                        %piece_offset,
                        "Failed to write piece into cache"
                    );
                } else {
                    trace!(
                        %cache_index,
                        %old_piece_index,
                        %piece_index,
                        %piece_offset,
                        "Successfully replaced old cached piece"
                    );
                    caches.push_stored_piece(record_key, offset);
                }
            }
            // There is free space in cache, need to find a free spot and place piece there
            None => {
                let Some(offset) = caches.pop_free_offset() else {
                    warn!(
                        %piece_index,
                        "Should have inserted piece into cache, but it didn't happen, this is an \
                        implementation bug"
                    );
                    return;
                };
                let cache_index = offset.cache_index;
                let piece_offset = offset.piece_offset;
                let Some(backend) = caches.get_backend(cache_index) else {
                    // Cache backend not exist
                    warn!(
                        %cache_index,
                        %piece_index,
                        "Should have a cached backend, but it didn't exist, this is an \
                        implementation bug"
                    );
                    return;
                };

                if let Err(error) = backend.write_piece(piece_offset, piece_index, &piece).await {
                    error!(
                        %error,
                        %cache_index,
                        %piece_index,
                        %piece_offset,
                        "Failed to write piece into cache"
                    );
                } else {
                    trace!(
                        %cache_index,
                        %piece_index,
                        %piece_offset,
                        "Successfully stored piece in cache"
                    );
                    if let Some(metrics) = &self.metrics {
                        metrics.piece_cache_capacity_used.inc();
                    }
                    caches.push_stored_piece(record_key, offset);
                }
            }
        };
    }
}

#[derive(Debug)]
struct PlotCaches {
    /// Additional piece caches
    caches: AsyncRwLock<Vec<Arc<dyn PlotCache>>>,
    /// Next plot cache to use for storing pieces
    next_plot_cache: AtomicUsize,
}

impl PlotCaches {
    async fn should_store(&self, piece_index: PieceIndex, key: &RecordKey) -> bool {
        for (cache_index, cache) in self.caches.read().await.iter().enumerate() {
            match cache.is_piece_maybe_stored(key).await {
                Ok(MaybePieceStoredResult::No) => {
                    // Try another one if there is any
                }
                Ok(MaybePieceStoredResult::Vacant) => {
                    return true;
                }
                Ok(MaybePieceStoredResult::Yes) => {
                    // Already stored, nothing else left to do
                    return false;
                }
                Err(error) => {
                    warn!(
                        %cache_index,
                        %piece_index,
                        %error,
                        "Failed to check piece stored in cache"
                    );
                }
            }
        }

        false
    }

    /// Store a piece in additional downloaded pieces, if there is space for them
    async fn store_additional_piece(&self, piece_index: PieceIndex, piece: &Piece) -> bool {
        let plot_caches = self.caches.read().await;
        let plot_caches_len = plot_caches.len();

        // Store pieces in plots using round-robin distribution
        for _ in 0..plot_caches_len {
            let plot_cache_index =
                self.next_plot_cache.fetch_add(1, Ordering::Relaxed) % plot_caches_len;

            match plot_caches[plot_cache_index]
                .try_store_piece(piece_index, piece)
                .await
            {
                Ok(true) => {
                    return false;
                }
                Ok(false) => {
                    continue;
                }
                Err(error) => {
                    error!(
                        %error,
                        %piece_index,
                        %plot_cache_index,
                        "Failed to store additional piece in cache"
                    );
                    continue;
                }
            }
        }

        false
    }
}

/// Farmer cache that aggregates different kinds of caches of multiple disks.
///
/// Pieces in [`PieceCache`] are stored based on capacity and proximity of piece index to farmer's
/// network identity. If capacity is not enough to store all pieces in cache then pieces that are
/// further from network identity will be evicted, this is helpful for quick retrieval of pieces
/// from DSN as well as plotting purposes.
///
/// [`PlotCache`] is used as a supplementary cache and is primarily helpful for smaller farmers
/// where piece cache is not enough to store all the pieces on the network, while there is a lot of
/// space in the plot that is not used by sectors yet and can be leverage as extra caching space.
#[derive(Debug, Clone)]
pub struct FarmerCache<CacheIndex> {
    peer_id: PeerId,
    /// Individual dedicated piece caches
    piece_caches: Arc<AsyncRwLock<PieceCachesState<CacheIndex>>>,
    /// Additional piece caches
    plot_caches: Arc<PlotCaches>,
    handlers: Arc<Handlers>,
    // We do not want to increase capacity unnecessarily on clone
    worker_sender: Arc<mpsc::Sender<WorkerCommand>>,
    metrics: Option<Arc<FarmerCacheMetrics>>,
}

impl<CacheIndex> FarmerCache<CacheIndex>
where
    CacheIndex: Hash + Eq + Copy + fmt::Debug + fmt::Display + Send + Sync + 'static,
    usize: From<CacheIndex>,
    CacheIndex: TryFrom<usize>,
{
    /// Create new piece cache instance and corresponding worker.
    ///
    /// NOTE: Returned future is async, but does blocking operations and should be running in
    /// dedicated thread.
    pub fn new<NC>(
        node_client: NC,
        peer_id: PeerId,
        registry: Option<&mut Registry>,
    ) -> (Self, FarmerCacheWorker<NC, CacheIndex>)
    where
        NC: NodeClient,
    {
        let caches = Arc::default();
        let (worker_sender, worker_receiver) = mpsc::channel(WORKER_CHANNEL_CAPACITY);
        let handlers = Arc::new(Handlers::default());

        let plot_caches = Arc::new(PlotCaches {
            caches: AsyncRwLock::default(),
            next_plot_cache: AtomicUsize::new(0),
        });
        let metrics = registry.map(|registry| Arc::new(FarmerCacheMetrics::new(registry)));

        let instance = Self {
            peer_id,
            piece_caches: Arc::clone(&caches),
            plot_caches: Arc::clone(&plot_caches),
            handlers: Arc::clone(&handlers),
            worker_sender: Arc::new(worker_sender),
            metrics: metrics.clone(),
        };
        let worker = FarmerCacheWorker {
            peer_id,
            node_client,
            piece_caches: caches,
            plot_caches,
            handlers,
            worker_receiver: Some(worker_receiver),
            metrics,
        };

        (instance, worker)
    }

    /// Get piece from cache
    pub async fn get_piece<Key>(&self, key: Key) -> Option<Piece>
    where
        RecordKey: From<Key>,
    {
        let key = RecordKey::from(key);
        let maybe_piece_found = {
            let caches = self.piece_caches.read().await;

            caches.get_stored_piece(&key).and_then(|offset| {
                let cache_index = offset.cache_index;
                let piece_offset = offset.piece_offset;
                Some((
                    piece_offset,
                    cache_index,
                    caches.get_backend(cache_index)?.clone(),
                ))
            })
        };

        if let Some((piece_offset, cache_index, backend)) = maybe_piece_found {
            match backend.read_piece(piece_offset).await {
                Ok(maybe_piece) => {
                    return match maybe_piece {
                        Some((_piece_index, piece)) => {
                            if let Some(metrics) = &self.metrics {
                                metrics.cache_get_hit.inc();
                            }
                            Some(piece)
                        }
                        None => {
                            if let Some(metrics) = &self.metrics {
                                metrics.cache_get_miss.inc();
                            }
                            None
                        }
                    };
                }
                Err(error) => {
                    error!(
                        %error,
                        %cache_index,
                        ?key,
                        %piece_offset,
                        "Error while reading piece from cache, might be a disk corruption"
                    );

                    if let Err(error) = self
                        .worker_sender
                        .send(WorkerCommand::ForgetKey { key })
                        .await
                    {
                        trace!(%error, "Failed to send ForgetKey command to worker");
                    }

                    if let Some(metrics) = &self.metrics {
                        metrics.cache_get_error.inc();
                    }
                    return None;
                }
            }
        }

        for cache in self.plot_caches.caches.read().await.iter() {
            if let Ok(Some(piece)) = cache.read_piece(&key).await {
                if let Some(metrics) = &self.metrics {
                    metrics.cache_get_hit.inc();
                }
                return Some(piece);
            }
        }

        if let Some(metrics) = &self.metrics {
            metrics.cache_get_miss.inc();
        }
        None
    }

    /// Find piece in cache and return its retrieval details
    pub(crate) async fn find_piece(
        &self,
        piece_index: PieceIndex,
    ) -> Option<(PieceCacheId, PieceCacheOffset)> {
        let key = RecordKey::from(piece_index.to_multihash());

        let caches = self.piece_caches.read().await;
        let Some(offset) = caches.get_stored_piece(&key) else {
            if let Some(metrics) = &self.metrics {
                metrics.cache_find_miss.inc();
            }

            return None;
        };
        let piece_offset = offset.piece_offset;

        if let Some(backend) = caches.get_backend(offset.cache_index) {
            if let Some(metrics) = &self.metrics {
                metrics.cache_find_hit.inc();
            }
            return Some((*backend.id(), piece_offset));
        }

        if let Some(metrics) = &self.metrics {
            metrics.cache_find_miss.inc();
        }
        None
    }

    /// Try to store a piece in additional downloaded pieces, if there is space for them
    pub async fn maybe_store_additional_piece(&self, piece_index: PieceIndex, piece: &Piece) {
        let key = RecordKey::from(piece_index.to_multihash());

        let should_store = self.plot_caches.should_store(piece_index, &key).await;

        if !should_store {
            return;
        }

        self.plot_caches
            .store_additional_piece(piece_index, piece)
            .await;
    }

    /// Initialize replacement of backing caches
    pub async fn replace_backing_caches(
        &self,
        new_piece_caches: Vec<Arc<dyn PieceCache>>,
        new_plot_caches: Vec<Arc<dyn PlotCache>>,
    ) {
        if let Err(error) = self
            .worker_sender
            .send(WorkerCommand::ReplaceBackingCaches { new_piece_caches })
            .await
        {
            warn!(%error, "Failed to replace backing caches, worker exited");
        }

        *self.plot_caches.caches.write().await = new_plot_caches;
    }

    /// Subscribe to cache sync notifications
    pub fn on_sync_progress(&self, callback: HandlerFn<f32>) -> HandlerId {
        self.handlers.progress.add(callback)
    }
}

impl<CacheIndex> LocalRecordProvider for FarmerCache<CacheIndex>
where
    CacheIndex: Hash + Eq + Copy + fmt::Debug + fmt::Display + Send + Sync + 'static,
    usize: From<CacheIndex>,
    CacheIndex: TryFrom<usize>,
{
    fn record(&self, key: &RecordKey) -> Option<ProviderRecord> {
        if self.piece_caches.try_read()?.contains_stored_piece(key) {
            // Note: We store our own provider records locally without local addresses
            // to avoid redundant storage and outdated addresses. Instead, these are
            // acquired on demand when returning a `ProviderRecord` for the local node.
            return Some(ProviderRecord {
                key: key.clone(),
                provider: self.peer_id,
                expires: None,
                addresses: Vec::new(),
            });
        };

        let found_fut = self
            .plot_caches
            .caches
            .try_read()?
            .iter()
            .map(|plot_cache| {
                let plot_cache = Arc::clone(plot_cache);

                async move {
                    matches!(
                        plot_cache.is_piece_maybe_stored(key).await,
                        Ok(MaybePieceStoredResult::Yes)
                    )
                }
            })
            .collect::<FuturesOrdered<_>>()
            .any(|found| async move { found });

        // TODO: Ideally libp2p would have an async API record store API,
        let found = block_in_place(|| {
            Handle::current()
                .block_on(tokio::time::timeout(
                    IS_PIECE_MAYBE_STORED_TIMEOUT,
                    found_fut,
                ))
                .unwrap_or_default()
        });

        // Note: We store our own provider records locally without local addresses
        // to avoid redundant storage and outdated addresses. Instead, these are
        // acquired on demand when returning a `ProviderRecord` for the local node.
        found.then_some(ProviderRecord {
            key: key.clone(),
            provider: self.peer_id,
            expires: None,
            addresses: Vec::new(),
        })
    }
}
