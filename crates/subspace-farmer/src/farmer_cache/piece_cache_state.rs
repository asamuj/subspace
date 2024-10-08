use crate::farmer_cache::{CacheBackend, FarmerCacheOffset};
use std::collections::hash_map::Values;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::hash::Hash;
use subspace_core_primitives::PieceIndex;
use subspace_networking::libp2p::kad::RecordKey;
use tracing::{debug, trace};

#[derive(Debug, Clone)]
pub(super) struct PieceCachesState<CacheIndex> {
    stored_pieces: HashMap<RecordKey, FarmerCacheOffset<CacheIndex>>,
    dangling_free_offsets: VecDeque<FarmerCacheOffset<CacheIndex>>,
    backends: Vec<CacheBackend>,
}

impl<CacheIndex> PieceCachesState<CacheIndex>
where
    CacheIndex: Hash + Eq + Copy + fmt::Debug + fmt::Display + Send + Sync + 'static,
    usize: From<CacheIndex>,
    CacheIndex: TryFrom<usize>,
{
    pub(super) fn new(
        stored_pieces: HashMap<RecordKey, FarmerCacheOffset<CacheIndex>>,
        dangling_free_offsets: VecDeque<FarmerCacheOffset<CacheIndex>>,
        backends: Vec<CacheBackend>,
    ) -> Self {
        Self {
            stored_pieces,
            dangling_free_offsets,
            backends,
        }
    }

    pub(super) fn total_capacity(&self) -> usize {
        self.backends()
            .fold(0usize, |acc, backend| acc + backend.total_capacity as usize)
    }

    pub(super) fn pop_free_offset(&mut self) -> Option<FarmerCacheOffset<CacheIndex>> {
        match self.dangling_free_offsets.pop_front() {
            Some(free_offset) => {
                debug!(?free_offset, "Popped dangling free offset");
                Some(free_offset)
            }
            None => {
                // Sort piece caches by number of stored pieces to fill those that are less
                // populated first
                let mut sorted_backends = self
                    .backends
                    .iter_mut()
                    .enumerate()
                    .filter_map(|(cache_index, backend)| {
                        Some((CacheIndex::try_from(cache_index).ok()?, backend))
                    })
                    .collect::<Vec<_>>();
                sorted_backends.sort_unstable_by_key(|(_, backend)| backend.free_size());
                sorted_backends
                    .into_iter()
                    .rev()
                    .find_map(|(cache_index, backend)| {
                        backend
                            .next_free()
                            .map(|free_offset| FarmerCacheOffset::new(cache_index, free_offset))
                    })
            }
        }
    }

    pub(super) fn get_stored_piece(
        &self,
        key: &RecordKey,
    ) -> Option<&FarmerCacheOffset<CacheIndex>> {
        self.stored_pieces.get(key)
    }

    pub(super) fn contains_stored_piece(&self, key: &RecordKey) -> bool {
        self.stored_pieces.contains_key(key)
    }

    pub(super) fn push_stored_piece(
        &mut self,
        key: RecordKey,
        cache_offset: FarmerCacheOffset<CacheIndex>,
    ) -> Option<FarmerCacheOffset<CacheIndex>> {
        self.stored_pieces.insert(key, cache_offset)
    }

    pub(super) fn stored_pieces_offests(
        &self,
    ) -> Values<'_, RecordKey, FarmerCacheOffset<CacheIndex>> {
        self.stored_pieces.values()
    }

    pub(super) fn remove_stored_piece(
        &mut self,
        key: &RecordKey,
    ) -> Option<FarmerCacheOffset<CacheIndex>> {
        self.stored_pieces.remove(key)
    }

    pub(super) fn free_unneeded_stored_pieces(
        &mut self,
        piece_indices_to_store: &mut HashMap<RecordKey, PieceIndex>,
    ) {
        self.stored_pieces
            .extract_if(|key, _offset| piece_indices_to_store.remove(key).is_none())
            .for_each(|(_piece_index, offset)| {
                // There is no need to adjust the `last_stored_offset` of the `backend` here,
                // as the free_offset will be preferentially taken from the dangling free offsets
                self.dangling_free_offsets.push_back(offset);
            })
    }

    pub(super) fn push_dangling_free_offset(&mut self, offset: FarmerCacheOffset<CacheIndex>) {
        trace!(?offset, "Pushing dangling free offset");
        self.dangling_free_offsets.push_back(offset);
    }

    pub(super) fn get_backend(&self, cache_index: CacheIndex) -> Option<&CacheBackend> {
        self.backends.get(usize::from(cache_index))
    }

    pub(super) fn backends(&self) -> impl ExactSizeIterator<Item = &CacheBackend> {
        self.backends.iter()
    }

    pub(super) fn reuse(
        self,
    ) -> (
        HashMap<RecordKey, FarmerCacheOffset<CacheIndex>>,
        VecDeque<FarmerCacheOffset<CacheIndex>>,
    ) {
        let Self {
            mut stored_pieces,
            mut dangling_free_offsets,
            backends: _,
        } = self;

        stored_pieces.clear();
        dangling_free_offsets.clear();
        (stored_pieces, dangling_free_offsets)
    }
}

impl<CacheIndex> Default for PieceCachesState<CacheIndex> {
    fn default() -> Self {
        Self {
            stored_pieces: HashMap::default(),
            dangling_free_offsets: VecDeque::default(),
            backends: Vec::default(),
        }
    }
}
