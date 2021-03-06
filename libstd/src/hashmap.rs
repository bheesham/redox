// Copyright 2014-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use self::Entry::*;
use self::SearchResult::*;
use self::VacantEntryState::*;

use borrow::Borrow;
use clone::Clone;
use cmp::{max, Eq, PartialEq};
use default::Default;
use fmt::{self, Debug};
use hash::{Hash, SipHasher};
use iter::{self, Iterator, ExactSizeIterator, IntoIterator, FromIterator, Extend, Map};
use marker::Sized;
use mem::{self, replace};
use ops::{Deref, FnMut, FnOnce, Index};
use option::Option::{self, Some, None};
use rand::{Rng, SeedableRng};

mod state {

    // Copyright 2014 The Rust Project Developers. See the COPYRIGHT
    // file at the top-level directory of this distribution and at
    // http://rust-lang.org/COPYRIGHT.
    //
    // Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
    // http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
    // <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
    // option. This file may not be copied, modified, or distributed
    // except according to those terms.



    use clone::Clone;
    use default::Default;
    use hash;
    use marker;

    /// A trait representing stateful hashes which can be used to hash keys in a
    /// `HashMap`.
    ///
    /// A HashState is used as a factory for instances of `Hasher` which a `HashMap`
    /// can then use to hash keys independently. A `HashMap` by default uses a state
    /// which will create instances of a `SipHasher`, but a custom state factory can
    /// be provided to the `with_hash_state` function.
    ///
    /// If a hashing algorithm has no initial state, then the `Hasher` type for that
    /// algorithm can implement the `Default` trait and create hash maps with the
    /// `DefaultState` structure. This state is 0-sized and will simply delegate
    /// to `Default` when asked to create a hasher.
    pub trait HashState {
        /// Type of the hasher that will be created.
        type Hasher: hash::Hasher;

        /// Creates a new hasher based on the given state of this object.
        fn hasher(&self) -> Self::Hasher;
    }

    /// A structure which is a factory for instances of `Hasher` which implement the
    /// default trait.
    ///
    /// This struct is 0-sized and does not need construction.
    pub struct DefaultState<H>(marker::PhantomData<H>);

    impl<H: Default + hash::Hasher> HashState for DefaultState<H> {
        type Hasher = H;
        fn hasher(&self) -> H { Default::default() }
    }

    impl<H> Clone for DefaultState<H> {
        fn clone(&self) -> DefaultState<H> { DefaultState(marker::PhantomData) }
    }

    impl<H> Default for DefaultState<H> {
        fn default() -> DefaultState<H> { DefaultState(marker::PhantomData) }
    }
}

mod raw_table {
    use alloc::heap::{allocate, deallocate, EMPTY};

    use cmp;
    use hash::{Hash, Hasher};
    use marker;
    use mem::{align_of, size_of};
    use mem;
    use num::wrapping::OverflowingOps;
    use ops::{Deref, DerefMut};
    use ptr::{self, Unique};
    use super::state::HashState;

    use self::BucketState::*;

    const EMPTY_BUCKET: u64 = 0;

    /// The raw hashtable, providing safe-ish access to the unzipped and highly
    /// optimized arrays of hashes, keys, and values.
    ///
    /// This design uses less memory and is a lot faster than the naive
    /// `Vec<Option<u64, K, V>>`, because we don't pay for the overhead of an
    /// option on every element, and we get a generally more cache-aware design.
    ///
    /// Essential invariants of this structure:
    ///
    ///   - if t.hashes[i] == EMPTY_BUCKET, then `Bucket::at_index(&t, i).raw`
    ///     points to 'undefined' contents. Don't read from it. This invariant is
    ///     enforced outside this module with the `EmptyBucket`, `FullBucket`,
    ///     and `SafeHash` types.
    ///
    ///   - An `EmptyBucket` is only constructed at an index with
    ///     a hash of EMPTY_BUCKET.
    ///
    ///   - A `FullBucket` is only constructed at an index with a
    ///     non-EMPTY_BUCKET hash.
    ///
    ///   - A `SafeHash` is only constructed for non-`EMPTY_BUCKET` hash. We get
    ///     around hashes of zero by changing them to 0x8000_0000_0000_0000,
    ///     which will likely map to the same bucket, while not being confused
    ///     with "empty".
    ///
    ///   - All three "arrays represented by pointers" are the same length:
    ///     `capacity`. This is set at creation and never changes. The arrays
    ///     are unzipped to save space (we don't have to pay for the padding
    ///     between odd sized elements, such as in a map from u64 to u8), and
    ///     be more cache aware (scanning through 8 hashes brings in at most
    ///     2 cache lines, since they're all right beside each other).
    ///
    /// You can kind of think of this module/data structure as a safe wrapper
    /// around just the "table" part of the hashtable. It enforces some
    /// invariants at the type level and employs some performance trickery,
    /// but in general is just a tricked out `Vec<Option<u64, K, V>>`.
#[unsafe_no_drop_flag]
    pub struct RawTable<K, V> {
        capacity: usize,
        size:     usize,
        hashes:   Unique<u64>,

        // Because K/V do not appear directly in any of the types in the struct,
        // inform rustc that in fact instances of K and V are reachable from here.
        marker:   marker::PhantomData<(K,V)>,
    }

    unsafe impl<K: Send, V: Send> Send for RawTable<K, V> {}
    unsafe impl<K: Sync, V: Sync> Sync for RawTable<K, V> {}

    struct RawBucket<K, V> {
        hash: *mut u64,
        key:  *mut K,
        val:  *mut V,
        _marker: marker::PhantomData<(K,V)>,
    }

    impl<K,V> Copy for RawBucket<K,V> {}
    impl<K,V> Clone for RawBucket<K,V> {
        fn clone(&self) -> RawBucket<K, V> { *self }
    }

    pub struct Bucket<K, V, M> {
        raw:   RawBucket<K, V>,
        idx:   usize,
        table: M
    }

    impl<K,V,M:Copy> Copy for Bucket<K,V,M> {}
    impl<K,V,M:Copy> Clone for Bucket<K,V,M> {
        fn clone(&self) -> Bucket<K,V,M> { *self }
    }

    pub struct EmptyBucket<K, V, M> {
        raw:   RawBucket<K, V>,
        idx:   usize,
        table: M
    }

    pub struct FullBucket<K, V, M> {
        raw:   RawBucket<K, V>,
        idx:   usize,
        table: M
    }

    pub type EmptyBucketImm<'table, K, V> = EmptyBucket<K, V, &'table RawTable<K, V>>;
    pub type  FullBucketImm<'table, K, V> =  FullBucket<K, V, &'table RawTable<K, V>>;

    pub type EmptyBucketMut<'table, K, V> = EmptyBucket<K, V, &'table mut RawTable<K, V>>;
    pub type  FullBucketMut<'table, K, V> =  FullBucket<K, V, &'table mut RawTable<K, V>>;

    pub enum BucketState<K, V, M> {
        Empty(EmptyBucket<K, V, M>),
        Full(FullBucket<K, V, M>),
    }

    // A GapThenFull encapsulates the state of two consecutive buckets at once.
    // The first bucket, called the gap, is known to be empty.
    // The second bucket is full.
    pub struct GapThenFull<K, V, M> {
        gap: EmptyBucket<K, V, ()>,
        full: FullBucket<K, V, M>,
    }

    /// A hash that is not zero, since we use a hash of zero to represent empty
    /// buckets.
#[derive(PartialEq, Copy, Clone)]
    pub struct SafeHash {
        hash: u64,
    }

    impl SafeHash {
        /// Peek at the hash value, which is guaranteed to be non-zero.
        #[inline(always)]
        pub fn inspect(&self) -> u64 { self.hash }
    }

    /// We need to remove hashes of 0. That's reserved for empty buckets.
    /// This function wraps up `hash_keyed` to be the only way outside this
    /// module to generate a SafeHash.
    pub fn make_hash<T: ?Sized, S>(hash_state: &S, t: &T) -> SafeHash
        where T: Hash, S: HashState
        {
            let mut state = hash_state.hasher();
            t.hash(&mut state);
            // We need to avoid 0 in order to prevent collisions with
            // EMPTY_HASH. We can maintain our precious uniform distribution
            // of initial indexes by unconditionally setting the MSB,
            // effectively reducing 64-bits hashes to 63 bits.
            SafeHash { hash: 0x8000_0000_0000_0000 | state.finish() }
        }

    // `replace` casts a `*u64` to a `*SafeHash`. Since we statically
    // ensure that a `FullBucket` points to an index with a non-zero hash,
    // and a `SafeHash` is just a `u64` with a different name, this is
    // safe.
    //
    // This test ensures that a `SafeHash` really IS the same size as a
    // `u64`. If you need to change the size of `SafeHash` (and
    // consequently made this test fail), `replace` needs to be
    // modified to no longer assume this.
#[test]
    fn can_alias_safehash_as_u64() {
        assert_eq!(size_of::<SafeHash>(), size_of::<u64>())
    }

    impl<K, V> RawBucket<K, V> {
        unsafe fn offset(self, count: isize) -> RawBucket<K, V> {
            RawBucket {
                hash: self.hash.offset(count),
                key:  self.key.offset(count),
                val:  self.val.offset(count),
                _marker: marker::PhantomData,
            }
        }
    }

    // Buckets hold references to the table.
    impl<K, V, M> FullBucket<K, V, M> {
        /// Borrow a reference to the table.
        pub fn table(&self) -> &M {
            &self.table
        }
        /// Move out the reference to the table.
        pub fn into_table(self) -> M {
            self.table
        }
        /// Get the raw index.
        pub fn index(&self) -> usize {
            self.idx
        }
    }

    impl<K, V, M> EmptyBucket<K, V, M> {
        /// Borrow a reference to the table.
        pub fn table(&self) -> &M {
            &self.table
        }
        /// Move out the reference to the table.
        pub fn into_table(self) -> M {
            self.table
        }
    }

    impl<K, V, M> Bucket<K, V, M> {
        /// Move out the reference to the table.
        pub fn into_table(self) -> M {
            self.table
        }
        /// Get the raw index.
        pub fn index(&self) -> usize {
            self.idx
        }
    }

    impl<K, V, M: Deref<Target=RawTable<K, V>>> Bucket<K, V, M> {
        pub fn new(table: M, hash: SafeHash) -> Bucket<K, V, M> {
            Bucket::at_index(table, hash.inspect() as usize)
        }

        pub fn at_index(table: M, ib_index: usize) -> Bucket<K, V, M> {
            // if capacity is 0, then the RawBucket will be populated with bogus pointers.
            // This is an uncommon case though, so avoid it in release builds.
            debug_assert!(table.capacity() > 0, "Table should have capacity at this point");
            let ib_index = ib_index & (table.capacity() - 1);
            Bucket {
                raw: unsafe {
                    table.first_bucket_raw().offset(ib_index as isize)
                },
                idx: ib_index,
                table: table
            }
        }

        pub fn first(table: M) -> Bucket<K, V, M> {
            Bucket {
                raw: table.first_bucket_raw(),
                idx: 0,
                table: table
            }
        }

        /// Reads a bucket at a given index, returning an enum indicating whether
        /// it's initialized or not. You need to match on this enum to get
        /// the appropriate types to call most of the other functions in
        /// this module.
        pub fn peek(self) -> BucketState<K, V, M> {
            match unsafe { *self.raw.hash } {
                EMPTY_BUCKET =>
                    Empty(EmptyBucket {
                        raw: self.raw,
                        idx: self.idx,
                        table: self.table
                    }),
                    _ =>
                        Full(FullBucket {
                            raw: self.raw,
                            idx: self.idx,
                            table: self.table
                        })
            }
        }

        /// Modifies the bucket pointer in place to make it point to the next slot.
        pub fn next(&mut self) {
            // Branchless bucket iteration step.
            // As we reach the end of the table...
            // We take the current idx:          0111111b
            // Xor it by its increment:        ^ 1000000b
            //                               ------------
            //                                   1111111b
            // Then AND with the capacity:     & 1000000b
            //                               ------------
            // to get the backwards offset:      1000000b
            // ... and it's zero at all other times.
            let maybe_wraparound_dist = (self.idx ^ (self.idx + 1)) & self.table.capacity();
            // Finally, we obtain the offset 1 or the offset -cap + 1.
            let dist = 1 - (maybe_wraparound_dist as isize);

            self.idx += 1;

            unsafe {
                self.raw = self.raw.offset(dist);
            }
        }
    }

    impl<K, V, M: Deref<Target=RawTable<K, V>>> EmptyBucket<K, V, M> {
        #[inline]
        pub fn next(self) -> Bucket<K, V, M> {
            let mut bucket = self.into_bucket();
            bucket.next();
            bucket
        }

        #[inline]
        pub fn into_bucket(self) -> Bucket<K, V, M> {
            Bucket {
                raw: self.raw,
                idx: self.idx,
                table: self.table
            }
        }

        pub fn gap_peek(self) -> Option<GapThenFull<K, V, M>> {
            let gap = EmptyBucket {
                raw: self.raw,
                idx: self.idx,
                table: ()
            };

            match self.next().peek() {
                Full(bucket) => {
                    Some(GapThenFull {
                        gap: gap,
                        full: bucket
                    })
                }
                Empty(..) => None
            }
        }
    }

    impl<K, V, M: Deref<Target=RawTable<K, V>> + DerefMut> EmptyBucket<K, V, M> {
        /// Puts given key and value pair, along with the key's hash,
        /// into this bucket in the hashtable. Note how `self` is 'moved' into
        /// this function, because this slot will no longer be empty when
        /// we return! A `FullBucket` is returned for later use, pointing to
        /// the newly-filled slot in the hashtable.
        ///
        /// Use `make_hash` to construct a `SafeHash` to pass to this function.
        pub fn put(mut self, hash: SafeHash, key: K, value: V)
            -> FullBucket<K, V, M> {
                unsafe {
                    *self.raw.hash = hash.inspect();
                    ptr::write(self.raw.key, key);
                    ptr::write(self.raw.val, value);
                }

                self.table.size += 1;

                FullBucket { raw: self.raw, idx: self.idx, table: self.table }
            }
    }

    impl<K, V, M: Deref<Target=RawTable<K, V>>> FullBucket<K, V, M> {
        #[inline]
        pub fn next(self) -> Bucket<K, V, M> {
            let mut bucket = self.into_bucket();
            bucket.next();
            bucket
        }

        #[inline]
        pub fn into_bucket(self) -> Bucket<K, V, M> {
            Bucket {
                raw: self.raw,
                idx: self.idx,
                table: self.table
            }
        }

        /// Get the distance between this bucket and the 'ideal' location
        /// as determined by the key's hash stored in it.
        ///
        /// In the cited blog posts above, this is called the "distance to
        /// initial bucket", or DIB. Also known as "probe count".
        pub fn distance(&self) -> usize {
            // Calculates the distance one has to travel when going from
            // `hash mod capacity` onwards to `idx mod capacity`, wrapping around
            // if the destination is not reached before the end of the table.
            (self.idx.wrapping_sub(self.hash().inspect() as usize)) & (self.table.capacity() - 1)
        }

        #[inline]
        pub fn hash(&self) -> SafeHash {
            unsafe {
                SafeHash {
                    hash: *self.raw.hash
                }
            }
        }

        /// Gets references to the key and value at a given index.
        pub fn read(&self) -> (&K, &V) {
            unsafe {
                (&*self.raw.key,
                 &*self.raw.val)
            }
        }
    }

    impl<K, V, M: Deref<Target=RawTable<K, V>> + DerefMut> FullBucket<K, V, M> {
        /// Removes this bucket's key and value from the hashtable.
        ///
        /// This works similarly to `put`, building an `EmptyBucket` out of the
        /// taken bucket.
        pub fn take(mut self) -> (EmptyBucket<K, V, M>, K, V) {
            self.table.size -= 1;

            unsafe {
                *self.raw.hash = EMPTY_BUCKET;
                (
                    EmptyBucket {
                        raw: self.raw,
                        idx: self.idx,
                        table: self.table
                    },
                    ptr::read(self.raw.key),
                    ptr::read(self.raw.val)
                )
            }
        }

        pub fn replace(&mut self, h: SafeHash, k: K, v: V) -> (SafeHash, K, V) {
            unsafe {
                let old_hash = ptr::replace(self.raw.hash as *mut SafeHash, h);
                let old_key  = ptr::replace(self.raw.key,  k);
                let old_val  = ptr::replace(self.raw.val,  v);

                (old_hash, old_key, old_val)
            }
        }

        /// Gets mutable references to the key and value at a given index.
        pub fn read_mut(&mut self) -> (&mut K, &mut V) {
            unsafe {
                (&mut *self.raw.key,
                 &mut *self.raw.val)
            }
        }
    }

    impl<'t, K, V, M: Deref<Target=RawTable<K, V>> + 't> FullBucket<K, V, M> {
        /// Exchange a bucket state for immutable references into the table.
        /// Because the underlying reference to the table is also consumed,
        /// no further changes to the structure of the table are possible;
        /// in exchange for this, the returned references have a longer lifetime
        /// than the references returned by `read()`.
        pub fn into_refs(self) -> (&'t K, &'t V) {
            unsafe {
                (&*self.raw.key,
                 &*self.raw.val)
            }
        }
    }

    impl<'t, K, V, M: Deref<Target=RawTable<K, V>> + DerefMut + 't> FullBucket<K, V, M> {
        /// This works similarly to `into_refs`, exchanging a bucket state
        /// for mutable references into the table.
        pub fn into_mut_refs(self) -> (&'t mut K, &'t mut V) {
            unsafe {
                (&mut *self.raw.key,
                 &mut *self.raw.val)
            }
        }
    }

    impl<K, V, M> BucketState<K, V, M> {
        // For convenience.
        pub fn expect_full(self) -> FullBucket<K, V, M> {
            match self {
                Full(full) => full,
                Empty(..) => panic!("Expected full bucket")
            }
        }
    }

    impl<K, V, M: Deref<Target=RawTable<K, V>>> GapThenFull<K, V, M> {
        #[inline]
        pub fn full(&self) -> &FullBucket<K, V, M> {
            &self.full
        }

        pub fn shift(mut self) -> Option<GapThenFull<K, V, M>> {
            unsafe {
                *self.gap.raw.hash = mem::replace(&mut *self.full.raw.hash, EMPTY_BUCKET);
                ptr::copy_nonoverlapping(self.full.raw.key, self.gap.raw.key, 1);
                ptr::copy_nonoverlapping(self.full.raw.val, self.gap.raw.val, 1);
            }

            let FullBucket { raw: prev_raw, idx: prev_idx, .. } = self.full;

            match self.full.next().peek() {
                Full(bucket) => {
                    self.gap.raw = prev_raw;
                    self.gap.idx = prev_idx;

                    self.full = bucket;

                    Some(self)
                }
                Empty(..) => None
            }
        }
    }


    /// Rounds up to a multiple of a power of two. Returns the closest multiple
    /// of `target_alignment` that is higher or equal to `unrounded`.
    ///
    /// # Panics
    ///
    /// Panics if `target_alignment` is not a power of two.
#[inline]
    fn round_up_to_next(unrounded: usize, target_alignment: usize) -> usize {
        assert!(target_alignment.is_power_of_two());
        (unrounded + target_alignment - 1) & !(target_alignment - 1)
    }

#[test]
    fn test_rounding() {
        assert_eq!(round_up_to_next(0, 4), 0);
        assert_eq!(round_up_to_next(1, 4), 4);
        assert_eq!(round_up_to_next(2, 4), 4);
        assert_eq!(round_up_to_next(3, 4), 4);
        assert_eq!(round_up_to_next(4, 4), 4);
        assert_eq!(round_up_to_next(5, 4), 8);
    }

    // Returns a tuple of (key_offset, val_offset),
    // from the start of a mallocated array.
#[inline]
    fn calculate_offsets(hashes_size: usize,
                         keys_size: usize, keys_align: usize,
                         vals_align: usize)
        -> (usize, usize, bool) {
            let keys_offset = round_up_to_next(hashes_size, keys_align);
            let (end_of_keys, oflo) = keys_offset.overflowing_add(keys_size);

            let vals_offset = round_up_to_next(end_of_keys, vals_align);

            (keys_offset, vals_offset, oflo)
        }

    // Returns a tuple of (minimum required malloc alignment, hash_offset,
    // array_size), from the start of a mallocated array.
    fn calculate_allocation(hash_size: usize, hash_align: usize,
                            keys_size: usize, keys_align: usize,
                            vals_size: usize, vals_align: usize)
        -> (usize, usize, usize, bool) {
            let hash_offset = 0;
            let (_, vals_offset, oflo) = calculate_offsets(hash_size,
                                                           keys_size, keys_align,
                                                           vals_align);
            let (end_of_vals, oflo2) = vals_offset.overflowing_add(vals_size);

            let align = cmp::max(hash_align, cmp::max(keys_align, vals_align));

            (align, hash_offset, end_of_vals, oflo || oflo2)
        }

#[test]
    fn test_offset_calculation() {
        assert_eq!(calculate_allocation(128, 8, 15, 1, 4,  4), (8, 0, 148, false));
        assert_eq!(calculate_allocation(3,   1, 2,  1, 1,  1), (1, 0, 6, false));
        assert_eq!(calculate_allocation(6,   2, 12, 4, 24, 8), (8, 0, 48, false));
        assert_eq!(calculate_offsets(128, 15, 1, 4), (128, 144, false));
        assert_eq!(calculate_offsets(3,   2,  1, 1), (3,   5, false));
        assert_eq!(calculate_offsets(6,   12, 4, 8), (8,   24, false));
    }

    impl<K, V> RawTable<K, V> {
        /// Does not initialize the buckets. The caller should ensure they,
        /// at the very least, set every hash to EMPTY_BUCKET.
        unsafe fn new_uninitialized(capacity: usize) -> RawTable<K, V> {
            if capacity == 0 {
                return RawTable {
                    size: 0,
                    capacity: 0,
                    hashes: Unique::new(EMPTY as *mut u64),
                    marker: marker::PhantomData,
                };
            }

            // No need for `checked_mul` before a more restrictive check performed
            // later in this method.
            let hashes_size = capacity * size_of::<u64>();
            let keys_size   = capacity * size_of::< K >();
            let vals_size   = capacity * size_of::< V >();

            // Allocating hashmaps is a little tricky. We need to allocate three
            // arrays, but since we know their sizes and alignments up front,
            // we just allocate a single array, and then have the subarrays
            // point into it.
            //
            // This is great in theory, but in practice getting the alignment
            // right is a little subtle. Therefore, calculating offsets has been
            // factored out into a different function.
            let (malloc_alignment, hash_offset, size, oflo) =
                calculate_allocation(
                    hashes_size, align_of::<u64>(),
                    keys_size,   align_of::< K >(),
                    vals_size,   align_of::< V >());

            assert!(!oflo, "capacity overflow");

            // One check for overflow that covers calculation and rounding of size.
            let size_of_bucket = size_of::<u64>().checked_add(size_of::<K>()).unwrap()
                .checked_add(size_of::<V>()).unwrap();
            assert!(size >= capacity.checked_mul(size_of_bucket)
                    .expect("capacity overflow"),
                    "capacity overflow");

            let buffer = allocate(size, malloc_alignment);
            if buffer.is_null() { ::alloc::oom() }

            let hashes = buffer.offset(hash_offset as isize) as *mut u64;

            RawTable {
                capacity: capacity,
                size:     0,
                hashes:   Unique::new(hashes),
                marker:   marker::PhantomData,
            }
        }

        fn first_bucket_raw(&self) -> RawBucket<K, V> {
            let hashes_size = self.capacity * size_of::<u64>();
            let keys_size = self.capacity * size_of::<K>();

            let buffer = *self.hashes as *mut u8;
            let (keys_offset, vals_offset, oflo) =
                calculate_offsets(hashes_size,
                                  keys_size, align_of::<K>(),
                                  align_of::<V>());
            debug_assert!(!oflo, "capacity overflow");
            unsafe {
                RawBucket {
                    hash: *self.hashes,
                    key:  buffer.offset(keys_offset as isize) as *mut K,
                    val:  buffer.offset(vals_offset as isize) as *mut V,
                    _marker: marker::PhantomData,
                }
            }
        }

        /// Creates a new raw table from a given capacity. All buckets are
        /// initially empty.
        pub fn new(capacity: usize) -> RawTable<K, V> {
            unsafe {
                let ret = RawTable::new_uninitialized(capacity);
                ptr::write_bytes(*ret.hashes, 0, capacity);
                ret
            }
        }

        /// The hashtable's capacity, similar to a vector's.
        pub fn capacity(&self) -> usize {
            self.capacity
        }

        /// The number of elements ever `put` in the hashtable, minus the number
        /// of elements ever `take`n.
        pub fn size(&self) -> usize {
            self.size
        }

        fn raw_buckets(&self) -> RawBuckets<K, V> {
            RawBuckets {
                raw: self.first_bucket_raw(),
                hashes_end: unsafe {
                    self.hashes.offset(self.capacity as isize)
                },
                marker: marker::PhantomData,
            }
        }

        pub fn iter(&self) -> Iter<K, V> {
            Iter {
                iter: self.raw_buckets(),
                elems_left: self.size(),
            }
        }

        pub fn iter_mut(&mut self) -> IterMut<K, V> {
            IterMut {
                iter: self.raw_buckets(),
                elems_left: self.size(),
            }
        }

        pub fn into_iter(self) -> IntoIter<K, V> {
            let RawBuckets { raw, hashes_end, .. } = self.raw_buckets();
            // Replace the marker regardless of lifetime bounds on parameters.
            IntoIter {
                iter: RawBuckets {
                    raw: raw,
                    hashes_end: hashes_end,
                    marker: marker::PhantomData,
                },
                table: self,
            }
        }

        pub fn drain(&mut self) -> Drain<K, V> {
            let RawBuckets { raw, hashes_end, .. } = self.raw_buckets();
            // Replace the marker regardless of lifetime bounds on parameters.
            Drain {
                iter: RawBuckets {
                    raw: raw,
                    hashes_end: hashes_end,
                    marker: marker::PhantomData,
                },
                table: self,
            }
        }

        /// Returns an iterator that copies out each entry. Used while the table
        /// is being dropped.
        unsafe fn rev_move_buckets(&mut self) -> RevMoveBuckets<K, V> {
            let raw_bucket = self.first_bucket_raw();
            RevMoveBuckets {
                raw: raw_bucket.offset(self.capacity as isize),
                hashes_end: raw_bucket.hash,
                elems_left: self.size,
                marker:     marker::PhantomData,
            }
        }
    }

    /// A raw iterator. The basis for some other iterators in this module. Although
    /// this interface is safe, it's not used outside this module.
    struct RawBuckets<'a, K, V> {
        raw: RawBucket<K, V>,
        hashes_end: *mut u64,

        // Strictly speaking, this should be &'a (K,V), but that would
        // require that K:'a, and we often use RawBuckets<'static...> for
        // move iterations, so that messes up a lot of other things. So
        // just use `&'a (K,V)` as this is not a publicly exposed type
        // anyway.
        marker: marker::PhantomData<&'a ()>,
    }

    // FIXME(#19839) Remove in favor of `#[derive(Clone)]`
    impl<'a, K, V> Clone for RawBuckets<'a, K, V> {
        fn clone(&self) -> RawBuckets<'a, K, V> {
            RawBuckets {
                raw: self.raw,
                hashes_end: self.hashes_end,
                marker: marker::PhantomData,
            }
        }
    }


    impl<'a, K, V> Iterator for RawBuckets<'a, K, V> {
        type Item = RawBucket<K, V>;

        fn next(&mut self) -> Option<RawBucket<K, V>> {
            while self.raw.hash != self.hashes_end {
                unsafe {
                    // We are swapping out the pointer to a bucket and replacing
                    // it with the pointer to the next one.
                    let prev = ptr::replace(&mut self.raw, self.raw.offset(1));
                    if *prev.hash != EMPTY_BUCKET {
                        return Some(prev);
                    }
                }
            }

            None
        }
    }

    /// An iterator that moves out buckets in reverse order. It leaves the table
    /// in an inconsistent state and should only be used for dropping
    /// the table's remaining entries. It's used in the implementation of Drop.
    struct RevMoveBuckets<'a, K, V> {
        raw: RawBucket<K, V>,
        hashes_end: *mut u64,
        elems_left: usize,

        // As above, `&'a (K,V)` would seem better, but we often use
        // 'static for the lifetime, and this is not a publicly exposed
        // type.
        marker: marker::PhantomData<&'a ()>,
    }

    impl<'a, K, V> Iterator for RevMoveBuckets<'a, K, V> {
        type Item = (K, V);

        fn next(&mut self) -> Option<(K, V)> {
            if self.elems_left == 0 {
                return None;
            }

            loop {
                debug_assert!(self.raw.hash != self.hashes_end);

                unsafe {
                    self.raw = self.raw.offset(-1);

                    if *self.raw.hash != EMPTY_BUCKET {
                        self.elems_left -= 1;
                        return Some((
                                ptr::read(self.raw.key),
                                ptr::read(self.raw.val)
                                ));
                    }
                }
            }
        }
    }

    /// Iterator over shared references to entries in a table.
    pub struct Iter<'a, K: 'a, V: 'a> {
        iter: RawBuckets<'a, K, V>,
        elems_left: usize,
    }

    unsafe impl<'a, K: Sync, V: Sync> Sync for Iter<'a, K, V> {}
    unsafe impl<'a, K: Sync, V: Sync> Send for Iter<'a, K, V> {}

    // FIXME(#19839) Remove in favor of `#[derive(Clone)]`
    impl<'a, K, V> Clone for Iter<'a, K, V> {
        fn clone(&self) -> Iter<'a, K, V> {
            Iter {
                iter: self.iter.clone(),
                elems_left: self.elems_left
            }
        }
    }


    /// Iterator over mutable references to entries in a table.
    pub struct IterMut<'a, K: 'a, V: 'a> {
        iter: RawBuckets<'a, K, V>,
        elems_left: usize,
    }

    unsafe impl<'a, K: Sync, V: Sync> Sync for IterMut<'a, K, V> {}
    // Both K: Sync and K: Send are correct for IterMut's Send impl,
    // but Send is the more useful bound
    unsafe impl<'a, K: Send, V: Send> Send for IterMut<'a, K, V> {}

    /// Iterator over the entries in a table, consuming the table.
    pub struct IntoIter<K, V> {
        table: RawTable<K, V>,
        iter: RawBuckets<'static, K, V>
    }

    unsafe impl<K: Sync, V: Sync> Sync for IntoIter<K, V> {}
    unsafe impl<K: Send, V: Send> Send for IntoIter<K, V> {}

    /// Iterator over the entries in a table, clearing the table.
    pub struct Drain<'a, K: 'a, V: 'a> {
        table: &'a mut RawTable<K, V>,
        iter: RawBuckets<'static, K, V>,
    }

    unsafe impl<'a, K: Sync, V: Sync> Sync for Drain<'a, K, V> {}
    unsafe impl<'a, K: Send, V: Send> Send for Drain<'a, K, V> {}

    impl<'a, K, V> Iterator for Iter<'a, K, V> {
        type Item = (&'a K, &'a V);

        fn next(&mut self) -> Option<(&'a K, &'a V)> {
            self.iter.next().map(|bucket| {
                self.elems_left -= 1;
                unsafe {
                    (&*bucket.key,
                     &*bucket.val)
                }
            })
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            (self.elems_left, Some(self.elems_left))
        }
    }
    impl<'a, K, V> ExactSizeIterator for Iter<'a, K, V> {
        fn len(&self) -> usize { self.elems_left }
    }

    impl<'a, K, V> Iterator for IterMut<'a, K, V> {
        type Item = (&'a K, &'a mut V);

        fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
            self.iter.next().map(|bucket| {
                self.elems_left -= 1;
                unsafe {
                    (&*bucket.key,
                     &mut *bucket.val)
                }
            })
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            (self.elems_left, Some(self.elems_left))
        }
    }
    impl<'a, K, V> ExactSizeIterator for IterMut<'a, K, V> {
        fn len(&self) -> usize { self.elems_left }
    }

    impl<K, V> Iterator for IntoIter<K, V> {
        type Item = (SafeHash, K, V);

        fn next(&mut self) -> Option<(SafeHash, K, V)> {
            self.iter.next().map(|bucket| {
                self.table.size -= 1;
                unsafe {
                    (
                        SafeHash {
                            hash: *bucket.hash,
                        },
                        ptr::read(bucket.key),
                        ptr::read(bucket.val)
                    )
                }
            })
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let size = self.table.size();
            (size, Some(size))
        }
    }
    impl<K, V> ExactSizeIterator for IntoIter<K, V> {
        fn len(&self) -> usize { self.table.size() }
    }

    impl<'a, K, V> Iterator for Drain<'a, K, V> {
        type Item = (SafeHash, K, V);

        #[inline]
        fn next(&mut self) -> Option<(SafeHash, K, V)> {
            self.iter.next().map(|bucket| {
                self.table.size -= 1;
                unsafe {
                    (
                        SafeHash {
                            hash: ptr::replace(bucket.hash, EMPTY_BUCKET),
                        },
                        ptr::read(bucket.key),
                        ptr::read(bucket.val)
                    )
                }
            })
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let size = self.table.size();
            (size, Some(size))
        }
    }
    impl<'a, K, V> ExactSizeIterator for Drain<'a, K, V> {
        fn len(&self) -> usize { self.table.size() }
    }

    impl<'a, K: 'a, V: 'a> Drop for Drain<'a, K, V> {
        fn drop(&mut self) {
            for _ in self {}
        }
    }

    impl<K: Clone, V: Clone> Clone for RawTable<K, V> {
        fn clone(&self) -> RawTable<K, V> {
            unsafe {
                let mut new_ht = RawTable::new_uninitialized(self.capacity());

                {
                    let cap = self.capacity();
                    let mut new_buckets = Bucket::first(&mut new_ht);
                    let mut buckets = Bucket::first(self);
                    while buckets.index() != cap {
                        match buckets.peek() {
                            Full(full) => {
                                let (h, k, v) = {
                                    let (k, v) = full.read();
                                    (full.hash(), k.clone(), v.clone())
                                };
                                *new_buckets.raw.hash = h.inspect();
                                ptr::write(new_buckets.raw.key, k);
                                ptr::write(new_buckets.raw.val, v);
                            }
                            Empty(..) => {
                                *new_buckets.raw.hash = EMPTY_BUCKET;
                            }
                        }
                        new_buckets.next();
                        buckets.next();
                    }
                };

                new_ht.size = self.size();

                new_ht
            }
        }
    }

    impl<K, V> Drop for RawTable<K, V> {
        #[unsafe_destructor_blind_to_params]
        fn drop(&mut self) {
            if self.capacity == 0 || self.capacity == mem::POST_DROP_USIZE {
                return;
            }

            // This is done in reverse because we've likely partially taken
            // some elements out with `.into_iter()` from the front.
            // Check if the size is 0, so we don't do a useless scan when
            // dropping empty tables such as on resize.
            // Also avoid double drop of elements that have been already moved out.
            unsafe {
                for _ in self.rev_move_buckets() {}
            }

            let hashes_size = self.capacity * size_of::<u64>();
            let keys_size = self.capacity * size_of::<K>();
            let vals_size = self.capacity * size_of::<V>();
            let (align, _, size, oflo) =
                calculate_allocation(hashes_size, align_of::<u64>(),
                keys_size, align_of::<K>(),
                vals_size, align_of::<V>());

            debug_assert!(!oflo, "should be impossible");

            unsafe {
                deallocate(*self.hashes as *mut u8, size, align);
                // Remember how everything was allocated out of one buffer
                // during initialization? We only need one call to free here.
            }
        }
    }
}

use self::raw_table::{
    Bucket,
    EmptyBucket,
    FullBucket,
    FullBucketImm,
    FullBucketMut,
    RawTable,
    SafeHash
};
use self::raw_table::BucketState::{
    Empty,
    Full,
};
use self::state::HashState;

const INITIAL_LOG2_CAP: usize = 5;
const INITIAL_CAPACITY: usize = 1 << INITIAL_LOG2_CAP; // 2^5

/// The default behavior of HashMap implements a load factor of 90.9%.
/// This behavior is characterized by the following condition:
///
/// - if size > 0.909 * capacity: grow the map
#[derive(Clone)]
struct DefaultResizePolicy;

impl DefaultResizePolicy {
    fn new() -> DefaultResizePolicy {
        DefaultResizePolicy
    }

    #[inline]
    fn min_capacity(&self, usable_size: usize) -> usize {
        // Here, we are rephrasing the logic by specifying the lower limit
        // on capacity:
        //
        // - if `cap < size * 1.1`: grow the map
        usable_size * 11 / 10
    }

    /// An inverse of `min_capacity`, approximately.
    #[inline]
    fn usable_capacity(&self, cap: usize) -> usize {
        // As the number of entries approaches usable capacity,
        // min_capacity(size) must be smaller than the internal capacity,
        // so that the map is not resized:
        // `min_capacity(usable_capacity(x)) <= x`.
        // The left-hand side can only be smaller due to flooring by integer
        // division.
        //
        // This doesn't have to be checked for overflow since allocation size
        // in bytes will overflow earlier than multiplication by 10.
        cap * 10 / 11
    }
}

#[test]
fn test_resize_policy() {
    let rp = DefaultResizePolicy;
    for n in 0..1000 {
        assert!(rp.min_capacity(rp.usable_capacity(n)) <= n);
        assert!(rp.usable_capacity(rp.min_capacity(n)) <= n);
    }
}

// The main performance trick in this hashmap is called Robin Hood Hashing.
// It gains its excellent performance from one essential operation:
//
//    If an insertion collides with an existing element, and that element's
//    "probe distance" (how far away the element is from its ideal location)
//    is higher than how far we've already probed, swap the elements.
//
// This massively lowers variance in probe distance, and allows us to get very
// high load factors with good performance. The 90% load factor I use is rather
// conservative.
//
// > Why a load factor of approximately 90%?
//
// In general, all the distances to initial buckets will converge on the mean.
// At a load factor of α, the odds of finding the target bucket after k
// probes is approximately 1-α^k. If we set this equal to 50% (since we converge
// on the mean) and set k=8 (64-byte cache line / 8-byte hash), α=0.92. I round
// this down to make the math easier on the CPU and avoid its FPU.
// Since on average we start the probing in the middle of a cache line, this
// strategy pulls in two cache lines of hashes on every lookup. I think that's
// pretty good, but if you want to trade off some space, it could go down to one
// cache line on average with an α of 0.84.
//
// > Wait, what? Where did you get 1-α^k from?
//
// On the first probe, your odds of a collision with an existing element is α.
// The odds of doing this twice in a row is approximately α^2. For three times,
// α^3, etc. Therefore, the odds of colliding k times is α^k. The odds of NOT
// colliding after k tries is 1-α^k.
//
// The paper from 1986 cited below mentions an implementation which keeps track
// of the distance-to-initial-bucket histogram. This approach is not suitable
// for modern architectures because it requires maintaining an internal data
// structure. This allows very good first guesses, but we are most concerned
// with guessing entire cache lines, not individual indexes. Furthermore, array
// accesses are no longer linear and in one direction, as we have now. There
// is also memory and cache pressure that this would entail that would be very
// difficult to properly see in a microbenchmark.
//
// ## Future Improvements (FIXME!)
//
// Allow the load factor to be changed dynamically and/or at initialization.
//
// Also, would it be possible for us to reuse storage when growing the
// underlying table? This is exactly the use case for 'realloc', and may
// be worth exploring.
//
// ## Future Optimizations (FIXME!)
//
// Another possible design choice that I made without any real reason is
// parameterizing the raw table over keys and values. Technically, all we need
// is the size and alignment of keys and values, and the code should be just as
// efficient (well, we might need one for power-of-two size and one for not...).
// This has the potential to reduce code bloat in rust executables, without
// really losing anything except 4 words (key size, key alignment, val size,
// val alignment) which can be passed in to every call of a `RawTable` function.
// This would definitely be an avenue worth exploring if people start complaining
// about the size of rust executables.
//
// Annotate exceedingly likely branches in `table::make_hash`
// and `search_hashed` to reduce instruction cache pressure
// and mispredictions once it becomes possible (blocked on issue #11092).
//
// Shrinking the table could simply reallocate in place after moving buckets
// to the first half.
//
// The growth algorithm (fragment of the Proof of Correctness)
// --------------------
//
// The growth algorithm is basically a fast path of the naive reinsertion-
// during-resize algorithm. Other paths should never be taken.
//
// Consider growing a robin hood hashtable of capacity n. Normally, we do this
// by allocating a new table of capacity `2n`, and then individually reinsert
// each element in the old table into the new one. This guarantees that the
// new table is a valid robin hood hashtable with all the desired statistical
// properties. Remark that the order we reinsert the elements in should not
// matter. For simplicity and efficiency, we will consider only linear
// reinsertions, which consist of reinserting all elements in the old table
// into the new one by increasing order of index. However we will not be
// starting our reinsertions from index 0 in general. If we start from index
// i, for the purpose of reinsertion we will consider all elements with real
// index j < i to have virtual index n + j.
//
// Our hash generation scheme consists of generating a 64-bit hash and
// truncating the most significant bits. When moving to the new table, we
// simply introduce a new bit to the front of the hash. Therefore, if an
// elements has ideal index i in the old table, it can have one of two ideal
// locations in the new table. If the new bit is 0, then the new ideal index
// is i. If the new bit is 1, then the new ideal index is n + i. Intuitively,
// we are producing two independent tables of size n, and for each element we
// independently choose which table to insert it into with equal probability.
// However the rather than wrapping around themselves on overflowing their
// indexes, the first table overflows into the first, and the first into the
// second. Visually, our new table will look something like:
//
// [yy_xxx_xxxx_xxx|xx_yyy_yyyy_yyy]
//
// Where x's are elements inserted into the first table, y's are elements
// inserted into the second, and _'s are empty sections. We now define a few
// key concepts that we will use later. Note that this is a very abstract
// perspective of the table. A real resized table would be at least half
// empty.
//
// Theorem: A linear robin hood reinsertion from the first ideal element
// produces identical results to a linear naive reinsertion from the same
// element.
//
// FIXME(Gankro, pczarn): review the proof and put it all in a separate README.md

/// A hash map implementation which uses linear probing with Robin
/// Hood bucket stealing.
///
/// The hashes are all keyed by the thread-local random number generator
/// on creation by default. This means that the ordering of the keys is
/// randomized, but makes the tables more resistant to
/// denial-of-service attacks (Hash DoS). This behavior can be
/// overridden with one of the constructors.
///
/// It is required that the keys implement the `Eq` and `Hash` traits, although
/// this can frequently be achieved by using `#[derive(PartialEq, Eq, Hash)]`.
/// If you implement these yourself, it is important that the following
/// property holds:
///
/// ```text
/// k1 == k2 -> hash(k1) == hash(k2)
/// ```
///
/// In other words, if two keys are equal, their hashes must be equal.
///
/// It is a logic error for a key to be modified in such a way that the key's
/// hash, as determined by the `Hash` trait, or its equality, as determined by
/// the `Eq` trait, changes while it is in the map. This is normally only
/// possible through `Cell`, `RefCell`, global state, I/O, or unsafe code.
///
/// Relevant papers/articles:
///
/// 1. Pedro Celis. ["Robin Hood Hashing"](https://cs.uwaterloo.ca/research/tr/1986/CS-86-14.pdf)
/// 2. Emmanuel Goossaert. ["Robin Hood
///    hashing"](http://codecapsule.com/2013/11/11/robin-hood-hashing/)
/// 3. Emmanuel Goossaert. ["Robin Hood hashing: backward shift
///    deletion"](http://codecapsule.com/2013/11/17/robin-hood-hashing-backward-shift-deletion/)
///
/// # Examples
///
/// ```
/// use std::collections::HashMap;
///
/// // type inference lets us omit an explicit type signature (which
/// // would be `HashMap<&str, &str>` in this example).
/// let mut book_reviews = HashMap::new();
///
/// // review some books.
/// book_reviews.insert("Adventures of Huckleberry Finn",    "My favorite book.");
/// book_reviews.insert("Grimms' Fairy Tales",               "Masterpiece.");
/// book_reviews.insert("Pride and Prejudice",               "Very enjoyable.");
/// book_reviews.insert("The Adventures of Sherlock Holmes", "Eye lyked it alot.");
///
/// // check for a specific one.
/// if !book_reviews.contains_key("Les Misérables") {
///     println!("We've got {} reviews, but Les Misérables ain't one.",
///              book_reviews.len());
/// }
///
/// // oops, this review has a lot of spelling mistakes, let's delete it.
/// book_reviews.remove("The Adventures of Sherlock Holmes");
///
/// // look up the values associated with some keys.
/// let to_find = ["Pride and Prejudice", "Alice's Adventure in Wonderland"];
/// for book in &to_find {
///     match book_reviews.get(book) {
///         Some(review) => println!("{}: {}", book, review),
///         None => println!("{} is unreviewed.", book)
///     }
/// }
///
/// // iterate over everything.
/// for (book, review) in &book_reviews {
///     println!("{}: \"{}\"", book, review);
/// }
/// ```
///
/// The easiest way to use `HashMap` with a custom type as key is to derive `Eq` and `Hash`.
/// We must also derive `PartialEq`.
///
/// ```
/// use std::collections::HashMap;
///
/// #[derive(Hash, Eq, PartialEq, Debug)]
/// struct Viking {
///     name: String,
///     country: String,
/// }
///
/// impl Viking {
///     /// Create a new Viking.
///     fn new(name: &str, country: &str) -> Viking {
///         Viking { name: name.to_string(), country: country.to_string() }
///     }
/// }
///
/// // Use a HashMap to store the vikings' health points.
/// let mut vikings = HashMap::new();
///
/// vikings.insert(Viking::new("Einar", "Norway"), 25);
/// vikings.insert(Viking::new("Olaf", "Denmark"), 24);
/// vikings.insert(Viking::new("Harald", "Iceland"), 12);
///
/// // Use derived implementation to print the status of the vikings.
/// for (viking, health) in &vikings {
///     println!("{:?} has {} hp", viking, health);
/// }
/// ```
#[derive(Clone)]

pub struct HashMap<K, V, S = RandomState> {
    // All hashes are keyed on these values, to prevent hash collision attacks.
    hash_state: S,

    table: RawTable<K, V>,

    resize_policy: DefaultResizePolicy,
}

/// Search for a pre-hashed key.
fn search_hashed<K, V, M, F>(table: M,
                             hash: SafeHash,
                             mut is_match: F)
    -> SearchResult<K, V, M> where
    M: Deref<Target=RawTable<K, V>>,
    F: FnMut(&K) -> bool,
{
    // This is the only function where capacity can be zero. To avoid
    // undefined behavior when Bucket::new gets the raw bucket in this
    // case, immediately return the appropriate search result.
    if table.capacity() == 0 {
        return TableRef(table);
    }

    let size = table.size();
    let mut probe = Bucket::new(table, hash);
    let ib = probe.index();

    while probe.index() != ib + size {
        let full = match probe.peek() {
            Empty(b) => return TableRef(b.into_table()), // hit an empty bucket
            Full(b) => b
        };

        if full.distance() + ib < full.index() {
            // We can finish the search early if we hit any bucket
            // with a lower distance to initial bucket than we've probed.
            return TableRef(full.into_table());
        }

        // If the hash doesn't match, it can't be this one..
        if hash == full.hash() {
            // If the key doesn't match, it can't be this one..
            if is_match(full.read().0) {
                return FoundExisting(full);
            }
        }

        probe = full.next();
    }

    TableRef(probe.into_table())
}

fn pop_internal<K, V>(starting_bucket: FullBucketMut<K, V>) -> (K, V) {
    let (empty, retkey, retval) = starting_bucket.take();
    let mut gap = match empty.gap_peek() {
        Some(b) => b,
        None => return (retkey, retval)
    };

    while gap.full().distance() != 0 {
        gap = match gap.shift() {
            Some(b) => b,
            None => break
        };
    }

    // Now we've done all our shifting. Return the value we grabbed earlier.
    (retkey, retval)
}

/// Perform robin hood bucket stealing at the given `bucket`. You must
/// also pass the position of that bucket's initial bucket so we don't have
/// to recalculate it.
///
/// `hash`, `k`, and `v` are the elements to "robin hood" into the hashtable.
fn robin_hood<'a, K: 'a, V: 'a>(mut bucket: FullBucketMut<'a, K, V>,
                                mut ib: usize,
                                mut hash: SafeHash,
                                mut k: K,
                                mut v: V)
-> &'a mut V {
    let starting_index = bucket.index();
    let size = {
        let table = bucket.table(); // FIXME "lifetime too short".
        table.size()
    };
    // There can be at most `size - dib` buckets to displace, because
    // in the worst case, there are `size` elements and we already are
    // `distance` buckets away from the initial one.
    let idx_end = starting_index + size - bucket.distance();

    loop {
        let (old_hash, old_key, old_val) = bucket.replace(hash, k, v);
        loop {
            let probe = bucket.next();
            assert!(probe.index() != idx_end);

            let full_bucket = match probe.peek() {
                Empty(bucket) => {
                    // Found a hole!
                    let b = bucket.put(old_hash, old_key, old_val);
                    // Now that it's stolen, just read the value's pointer
                    // right out of the table!
                    return Bucket::at_index(b.into_table(), starting_index)
                        .peek()
                        .expect_full()
                        .into_mut_refs()
                        .1;
                },
                Full(bucket) => bucket
            };

            let probe_ib = full_bucket.index() - full_bucket.distance();

            bucket = full_bucket;

            // Robin hood! Steal the spot.
            if ib < probe_ib {
                ib = probe_ib;
                hash = old_hash;
                k = old_key;
                v = old_val;
                break;
            }
        }
    }
}

/// A result that works like Option<FullBucket<..>> but preserves
/// the reference that grants us access to the table in any case.
enum SearchResult<K, V, M> {
    // This is an entry that holds the given key:
    FoundExisting(FullBucket<K, V, M>),

    // There was no such entry. The reference is given back:
    TableRef(M)
}

impl<K, V, M> SearchResult<K, V, M> {
    fn into_option(self) -> Option<FullBucket<K, V, M>> {
        match self {
            FoundExisting(bucket) => Some(bucket),
            TableRef(_) => None
        }
    }
}

impl<K, V, S> HashMap<K, V, S>
where K: Eq + Hash, S: HashState
{
    fn make_hash<X: ?Sized>(&self, x: &X) -> SafeHash where X: Hash {
        raw_table::make_hash(&self.hash_state, x)
    }

    /// Search for a key, yielding the index if it's found in the hashtable.
    /// If you already have the hash for the key lying around, use
    /// search_hashed.
    fn search<'a, Q: ?Sized>(&'a self, q: &Q) -> Option<FullBucketImm<'a, K, V>>
        where K: Borrow<Q>, Q: Eq + Hash
        {
            let hash = self.make_hash(q);
            search_hashed(&self.table, hash, |k| q.eq(k.borrow()))
                .into_option()
        }

    fn search_mut<'a, Q: ?Sized>(&'a mut self, q: &Q) -> Option<FullBucketMut<'a, K, V>>
        where K: Borrow<Q>, Q: Eq + Hash
        {
            let hash = self.make_hash(q);
            search_hashed(&mut self.table, hash, |k| q.eq(k.borrow()))
                .into_option()
        }

    // The caller should ensure that invariants by Robin Hood Hashing hold.
    fn insert_hashed_ordered(&mut self, hash: SafeHash, k: K, v: V) {
        let cap = self.table.capacity();
        let mut buckets = Bucket::new(&mut self.table, hash);
        let ib = buckets.index();

        while buckets.index() != ib + cap {
            // We don't need to compare hashes for value swap.
            // Not even DIBs for Robin Hood.
            buckets = match buckets.peek() {
                Empty(empty) => {
                    empty.put(hash, k, v);
                    return;
                }
                Full(b) => b.into_bucket()
            };
            buckets.next();
        }
        panic!("Internal HashMap error: Out of space.");
    }
}

impl<K: Hash + Eq, V> HashMap<K, V, RandomState> {
    /// Creates an empty HashMap.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// let mut map: HashMap<&str, isize> = HashMap::new();
    /// ```
    #[inline]

    pub fn new() -> HashMap<K, V, RandomState> {
        Default::default()
    }

    /// Creates an empty hash map with the given initial capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// let mut map: HashMap<&str, isize> = HashMap::with_capacity(10);
    /// ```
    #[inline]

    pub fn with_capacity(capacity: usize) -> HashMap<K, V, RandomState> {
        HashMap::with_capacity_and_hash_state(capacity, Default::default())
    }
}

impl<K, V, S> HashMap<K, V, S>
where K: Eq + Hash, S: HashState
{
    /// Creates an empty hashmap which will use the given hasher to hash keys.
    ///
    /// The created map has the default initial capacity.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(hashmap_hasher)]
    ///
    /// use std::collections::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let s = RandomState::new();
    /// let mut map = HashMap::with_hash_state(s);
    /// map.insert(1, 2);
    /// ```
    #[inline]

        pub fn with_hash_state(hash_state: S) -> HashMap<K, V, S> {
            HashMap {
                hash_state:    hash_state,
                resize_policy: DefaultResizePolicy::new(),
                table:         RawTable::new(0),
            }
        }

    /// Creates an empty HashMap with space for at least `capacity`
    /// elements, using `hasher` to hash the keys.
    ///
    /// Warning: `hasher` is normally randomly generated, and
    /// is designed to allow HashMaps to be resistant to attacks that
    /// cause many collisions and very poor performance. Setting it
    /// manually using this function can expose a DoS attack vector.
    ///
    /// # Examples
    ///
    /// ```
    /// #![feature(hashmap_hasher)]
    ///
    /// use std::collections::HashMap;
    /// use std::collections::hash_map::RandomState;
    ///
    /// let s = RandomState::new();
    /// let mut map = HashMap::with_capacity_and_hash_state(10, s);
    /// map.insert(1, 2);
    /// ```
    #[inline]

        pub fn with_capacity_and_hash_state(capacity: usize, hash_state: S)
        -> HashMap<K, V, S> {
            let resize_policy = DefaultResizePolicy::new();
            let min_cap = max(INITIAL_CAPACITY, resize_policy.min_capacity(capacity));
            let internal_cap = min_cap.checked_next_power_of_two().expect("capacity overflow");
            assert!(internal_cap >= capacity, "capacity overflow");
            HashMap {
                hash_state:    hash_state,
                resize_policy: resize_policy,
                table:         RawTable::new(internal_cap),
            }
        }

    /// Returns the number of elements the map can hold without reallocating.
    ///
    /// This number is a lower bound; the `HashMap<K, V>` might be able to hold
    /// more, but is guaranteed to be able to hold at least this many.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// let map: HashMap<isize, isize> = HashMap::with_capacity(100);
    /// assert!(map.capacity() >= 100);
    /// ```
    #[inline]

    pub fn capacity(&self) -> usize {
        self.resize_policy.usable_capacity(self.table.capacity())
    }

    /// Reserves capacity for at least `additional` more elements to be inserted
    /// in the `HashMap`. The collection may reserve more space to avoid
    /// frequent reallocations.
    ///
    /// # Panics
    ///
    /// Panics if the new allocation size overflows `usize`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    /// let mut map: HashMap<&str, isize> = HashMap::new();
    /// map.reserve(10);
    /// ```

    pub fn reserve(&mut self, additional: usize) {
        let new_size = self.len().checked_add(additional).expect("capacity overflow");
        let min_cap = self.resize_policy.min_capacity(new_size);

        // An invalid value shouldn't make us run out of space. This includes
        // an overflow check.
        assert!(new_size <= min_cap);

        if self.table.capacity() < min_cap {
            let new_capacity = max(min_cap.next_power_of_two(), INITIAL_CAPACITY);
            self.resize(new_capacity);
        }
    }

    /// Resizes the internal vectors to a new capacity. It's your responsibility to:
    ///   1) Make sure the new capacity is enough for all the elements, accounting
    ///      for the load factor.
    ///   2) Ensure new_capacity is a power of two or zero.
    fn resize(&mut self, new_capacity: usize) {
        assert!(self.table.size() <= new_capacity);
        assert!(new_capacity.is_power_of_two() || new_capacity == 0);

        let mut old_table = replace(&mut self.table, RawTable::new(new_capacity));
        let old_size = old_table.size();

        if old_table.capacity() == 0 || old_table.size() == 0 {
            return;
        }

        // Grow the table.
        // Specialization of the other branch.
        let mut bucket = Bucket::first(&mut old_table);

        // "So a few of the first shall be last: for many be called,
        // but few chosen."
        //
        // We'll most likely encounter a few buckets at the beginning that
        // have their initial buckets near the end of the table. They were
        // placed at the beginning as the probe wrapped around the table
        // during insertion. We must skip forward to a bucket that won't
        // get reinserted too early and won't unfairly steal others spot.
        // This eliminates the need for robin hood.
        loop {
            bucket = match bucket.peek() {
                Full(full) => {
                    if full.distance() == 0 {
                        // This bucket occupies its ideal spot.
                        // It indicates the start of another "cluster".
                        bucket = full.into_bucket();
                        break;
                    }
                    // Leaving this bucket in the last cluster for later.
                    full.into_bucket()
                }
                Empty(b) => {
                    // Encountered a hole between clusters.
                    b.into_bucket()
                }
            };
            bucket.next();
        }

        // This is how the buckets might be laid out in memory:
        // ($ marks an initialized bucket)
        //  ________________
        // |$$$_$$$$$$_$$$$$|
        //
        // But we've skipped the entire initial cluster of buckets
        // and will continue iteration in this order:
        //  ________________
        //     |$$$$$$_$$$$$
        //                  ^ wrap around once end is reached
        //  ________________
        //  $$$_____________|
        //    ^ exit once table.size == 0
        loop {
            bucket = match bucket.peek() {
                Full(bucket) => {
                    let h = bucket.hash();
                    let (b, k, v) = bucket.take();
                    self.insert_hashed_ordered(h, k, v);
                    {
                        let t = b.table(); // FIXME "lifetime too short".
                        if t.size() == 0 { break }
                    };
                    b.into_bucket()
                }
                Empty(b) => b.into_bucket()
            };
            bucket.next();
        }

        assert_eq!(self.table.size(), old_size);
    }

    /// Shrinks the capacity of the map as much as possible. It will drop
    /// down as much as possible while maintaining the internal rules
    /// and possibly leaving some space in accordance with the resize policy.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map: HashMap<isize, isize> = HashMap::with_capacity(100);
    /// map.insert(1, 2);
    /// map.insert(3, 4);
    /// assert!(map.capacity() >= 100);
    /// map.shrink_to_fit();
    /// assert!(map.capacity() >= 2);
    /// ```

    pub fn shrink_to_fit(&mut self) {
        let min_capacity = self.resize_policy.min_capacity(self.len());
        let min_capacity = max(min_capacity.next_power_of_two(), INITIAL_CAPACITY);

        // An invalid value shouldn't make us run out of space.
        debug_assert!(self.len() <= min_capacity);

        if self.table.capacity() != min_capacity {
            let old_table = replace(&mut self.table, RawTable::new(min_capacity));
            let old_size = old_table.size();

            // Shrink the table. Naive algorithm for resizing:
            for (h, k, v) in old_table.into_iter() {
                self.insert_hashed_nocheck(h, k, v);
            }

            debug_assert_eq!(self.table.size(), old_size);
        }
    }

    /// Insert a pre-hashed key-value pair, without first checking
    /// that there's enough room in the buckets. Returns a reference to the
    /// newly insert value.
    ///
    /// If the key already exists, the hashtable will be returned untouched
    /// and a reference to the existing element will be returned.
    fn insert_hashed_nocheck(&mut self, hash: SafeHash, k: K, v: V) -> &mut V {
        self.insert_or_replace_with(hash, k, v, |_, _, _, _| ())
    }

    fn insert_or_replace_with<'a, F>(&'a mut self,
                                     hash: SafeHash,
                                     k: K,
                                     v: V,
                                     mut found_existing: F)
        -> &'a mut V where
            F: FnMut(&mut K, &mut V, K, V),
        {
            // Worst case, we'll find one empty bucket among `size + 1` buckets.
            let size = self.table.size();
            let mut probe = Bucket::new(&mut self.table, hash);
            let ib = probe.index();

            loop {
                let mut bucket = match probe.peek() {
                    Empty(bucket) => {
                        // Found a hole!
                        return bucket.put(hash, k, v).into_mut_refs().1;
                    }
                    Full(bucket) => bucket
                };

                // hash matches?
                if bucket.hash() == hash {
                    // key matches?
                    if k == *bucket.read_mut().0 {
                        let (bucket_k, bucket_v) = bucket.into_mut_refs();
                        debug_assert!(k == *bucket_k);
                        // Key already exists. Get its reference.
                        found_existing(bucket_k, bucket_v, k, v);
                        return bucket_v;
                    }
                }

                let robin_ib = bucket.index() as isize - bucket.distance() as isize;

                if (ib as isize) < robin_ib {
                    // Found a luckier bucket than me. Better steal his spot.
                    return robin_hood(bucket, robin_ib as usize, hash, k, v);
                }

                probe = bucket.next();
                assert!(probe.index() != ib + size + 1);
            }
        }

    /// An iterator visiting all keys in arbitrary order.
    /// Iterator element type is `&'a K`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert("a", 1);
    /// map.insert("b", 2);
    /// map.insert("c", 3);
    ///
    /// for key in map.keys() {
    ///     println!("{}", key);
    /// }
    /// ```

    pub fn keys<'a>(&'a self) -> Keys<'a, K, V> {
        fn first<A, B>((a, _): (A, B)) -> A { a }
        let first: fn((&'a K,&'a V)) -> &'a K = first; // coerce to fn ptr

        Keys { inner: self.iter().map(first) }
    }

    /// An iterator visiting all values in arbitrary order.
    /// Iterator element type is `&'a V`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert("a", 1);
    /// map.insert("b", 2);
    /// map.insert("c", 3);
    ///
    /// for val in map.values() {
    ///     println!("{}", val);
    /// }
    /// ```

    pub fn values<'a>(&'a self) -> Values<'a, K, V> {
        fn second<A, B>((_, b): (A, B)) -> B { b }
        let second: fn((&'a K,&'a V)) -> &'a V = second; // coerce to fn ptr

        Values { inner: self.iter().map(second) }
    }

    /// An iterator visiting all key-value pairs in arbitrary order.
    /// Iterator element type is `(&'a K, &'a V)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert("a", 1);
    /// map.insert("b", 2);
    /// map.insert("c", 3);
    ///
    /// for (key, val) in map.iter() {
    ///     println!("key: {} val: {}", key, val);
    /// }
    /// ```

    pub fn iter(&self) -> Iter<K, V> {
        Iter { inner: self.table.iter() }
    }

    /// An iterator visiting all key-value pairs in arbitrary order,
    /// with mutable references to the values.
    /// Iterator element type is `(&'a K, &'a mut V)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert("a", 1);
    /// map.insert("b", 2);
    /// map.insert("c", 3);
    ///
    /// // Update all values
    /// for (_, val) in map.iter_mut() {
    ///     *val *= 2;
    /// }
    ///
    /// for (key, val) in &map {
    ///     println!("key: {} val: {}", key, val);
    /// }
    /// ```

    pub fn iter_mut(&mut self) -> IterMut<K, V> {
        IterMut { inner: self.table.iter_mut() }
    }

    /// Gets the given key's corresponding entry in the map for in-place manipulation.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut letters = HashMap::new();
    ///
    /// for ch in "a short treatise on fungi".chars() {
    ///     let counter = letters.entry(ch).or_insert(0);
    ///     *counter += 1;
    /// }
    ///
    /// assert_eq!(letters[&'s'], 2);
    /// assert_eq!(letters[&'t'], 3);
    /// assert_eq!(letters[&'u'], 1);
    /// assert_eq!(letters.get(&'y'), None);
    /// ```

    pub fn entry(&mut self, key: K) -> Entry<K, V> {
        // Gotta resize now.
        self.reserve(1);

        let hash = self.make_hash(&key);
        search_entry_hashed(&mut self.table, hash, key)
    }

    /// Returns the number of elements in the map.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut a = HashMap::new();
    /// assert_eq!(a.len(), 0);
    /// a.insert(1, "a");
    /// assert_eq!(a.len(), 1);
    /// ```

    pub fn len(&self) -> usize { self.table.size() }

    /// Returns true if the map contains no elements.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut a = HashMap::new();
    /// assert!(a.is_empty());
    /// a.insert(1, "a");
    /// assert!(!a.is_empty());
    /// ```
    #[inline]

    pub fn is_empty(&self) -> bool { self.len() == 0 }

    /// Clears the map, returning all key-value pairs as an iterator. Keeps the
    /// allocated memory for reuse.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut a = HashMap::new();
    /// a.insert(1, "a");
    /// a.insert(2, "b");
    ///
    /// for (k, v) in a.drain().take(1) {
    ///     assert!(k == 1 || k == 2);
    ///     assert!(v == "a" || v == "b");
    /// }
    ///
    /// assert!(a.is_empty());
    /// ```
    #[inline]

    pub fn drain(&mut self) -> Drain<K, V> {
        fn last_two<A, B, C>((_, b, c): (A, B, C)) -> (B, C) { (b, c) }
        let last_two: fn((SafeHash, K, V)) -> (K, V) = last_two; // coerce to fn pointer

        Drain {
            inner: self.table.drain().map(last_two),
        }
    }

    /// Clears the map, removing all key-value pairs. Keeps the allocated memory
    /// for reuse.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut a = HashMap::new();
    /// a.insert(1, "a");
    /// a.clear();
    /// assert!(a.is_empty());
    /// ```

    pub fn clear(&mut self) {
        self.drain();
    }

    /// Returns a reference to the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// `Hash` and `Eq` on the borrowed form *must* match those for
    /// the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert(1, "a");
    /// assert_eq!(map.get(&1), Some(&"a"));
    /// assert_eq!(map.get(&2), None);
    /// ```

    pub fn get<Q: ?Sized>(&self, k: &Q) -> Option<&V>
        where K: Borrow<Q>, Q: Hash + Eq
        {
            self.search(k).map(|bucket| bucket.into_refs().1)
        }

    /// Returns true if the map contains a value for the specified key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// `Hash` and `Eq` on the borrowed form *must* match those for
    /// the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert(1, "a");
    /// assert_eq!(map.contains_key(&1), true);
    /// assert_eq!(map.contains_key(&2), false);
    /// ```

    pub fn contains_key<Q: ?Sized>(&self, k: &Q) -> bool
        where K: Borrow<Q>, Q: Hash + Eq
        {
            self.search(k).is_some()
        }

    /// Returns a mutable reference to the value corresponding to the key.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// `Hash` and `Eq` on the borrowed form *must* match those for
    /// the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert(1, "a");
    /// if let Some(x) = map.get_mut(&1) {
    ///     *x = "b";
    /// }
    /// assert_eq!(map[&1], "b");
    /// ```

    pub fn get_mut<Q: ?Sized>(&mut self, k: &Q) -> Option<&mut V>
        where K: Borrow<Q>, Q: Hash + Eq
        {
            self.search_mut(k).map(|bucket| bucket.into_mut_refs().1)
        }

    /// Inserts a key-value pair into the map.
    ///
    /// If the map did not have this key present, `None` is returned.
    ///
    /// If the map did have this key present, the key is not updated, the
    /// value is updated and the old value is returned.
    /// See the [module-level documentation] for more.
    ///
    /// [module-level documentation]: index.html#insert-and-complex-keys
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// assert_eq!(map.insert(37, "a"), None);
    /// assert_eq!(map.is_empty(), false);
    ///
    /// map.insert(37, "b");
    /// assert_eq!(map.insert(37, "c"), Some("b"));
    /// assert_eq!(map[&37], "c");
    /// ```

    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let hash = self.make_hash(&k);
        self.reserve(1);

        let mut retval = None;
        self.insert_or_replace_with(hash, k, v, |_, val_ref, _, val| {
            retval = Some(replace(val_ref, val));
        });
        retval
    }

    /// Removes a key from the map, returning the value at the key if the key
    /// was previously in the map.
    ///
    /// The key may be any borrowed form of the map's key type, but
    /// `Hash` and `Eq` on the borrowed form *must* match those for
    /// the key type.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert(1, "a");
    /// assert_eq!(map.remove(&1), Some("a"));
    /// assert_eq!(map.remove(&1), None);
    /// ```

    pub fn remove<Q: ?Sized>(&mut self, k: &Q) -> Option<V>
        where K: Borrow<Q>, Q: Hash + Eq
        {
            if self.table.size() == 0 {
                return None
            }

            self.search_mut(k).map(|bucket| pop_internal(bucket).1)
        }
}

fn search_entry_hashed<'a, K: Eq, V>(table: &'a mut RawTable<K,V>, hash: SafeHash, k: K)
-> Entry<'a, K, V>
{
    // Worst case, we'll find one empty bucket among `size + 1` buckets.
    let size = table.size();
    let mut probe = Bucket::new(table, hash);
    let ib = probe.index();

    loop {
        let bucket = match probe.peek() {
            Empty(bucket) => {
                // Found a hole!
                return Vacant(VacantEntry {
                    hash: hash,
                    key: k,
                    elem: NoElem(bucket),
                });
            },
            Full(bucket) => bucket
        };

        // hash matches?
        if bucket.hash() == hash {
            // key matches?
            if k == *bucket.read().0 {
                return Occupied(OccupiedEntry{
                    elem: bucket,
                });
            }
        }

        let robin_ib = bucket.index() as isize - bucket.distance() as isize;

        if (ib as isize) < robin_ib {
            // Found a luckier bucket than me. Better steal his spot.
            return Vacant(VacantEntry {
                hash: hash,
                key: k,
                elem: NeqElem(bucket, robin_ib as usize),
            });
        }

        probe = bucket.next();
        assert!(probe.index() != ib + size + 1);
    }
}


impl<K, V, S> PartialEq for HashMap<K, V, S>
where K: Eq + Hash, V: PartialEq, S: HashState
{
    fn eq(&self, other: &HashMap<K, V, S>) -> bool {
        if self.len() != other.len() { return false; }

        self.iter().all(|(key, value)|
                        other.get(key).map_or(false, |v| *value == *v)
                       )
    }
}


impl<K, V, S> Eq for HashMap<K, V, S>
where K: Eq + Hash, V: Eq, S: HashState
{}


impl<K, V, S> Debug for HashMap<K, V, S>
where K: Eq + Hash + Debug, V: Debug, S: HashState
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}


impl<K, V, S> Default for HashMap<K, V, S>
where K: Eq + Hash,
      S: HashState + Default,
{
    fn default() -> HashMap<K, V, S> {
        HashMap::with_hash_state(Default::default())
    }
}


impl<'a, K, Q: ?Sized, V, S> Index<&'a Q> for HashMap<K, V, S>
where K: Eq + Hash + Borrow<Q>,
      Q: Eq + Hash,
      S: HashState,
{
    type Output = V;

    #[inline]
    fn index(&self, index: &Q) -> &V {
        self.get(index).expect("no entry found for key")
    }
}

/// HashMap iterator.

pub struct Iter<'a, K: 'a, V: 'a> {
    inner: raw_table::Iter<'a, K, V>
}

// FIXME(#19839) Remove in favor of `#[derive(Clone)]`

impl<'a, K, V> Clone for Iter<'a, K, V> {
    fn clone(&self) -> Iter<'a, K, V> {
        Iter {
            inner: self.inner.clone()
        }
    }
}

/// HashMap mutable values iterator.

pub struct IterMut<'a, K: 'a, V: 'a> {
    inner: raw_table::IterMut<'a, K, V>
}

/// HashMap move iterator.

pub struct IntoIter<K, V> {
    inner: iter::Map<raw_table::IntoIter<K, V>, fn((SafeHash, K, V)) -> (K, V)>
}

/// HashMap keys iterator.

pub struct Keys<'a, K: 'a, V: 'a> {
    inner: Map<Iter<'a, K, V>, fn((&'a K, &'a V)) -> &'a K>
}

// FIXME(#19839) Remove in favor of `#[derive(Clone)]`

impl<'a, K, V> Clone for Keys<'a, K, V> {
    fn clone(&self) -> Keys<'a, K, V> {
        Keys {
            inner: self.inner.clone()
        }
    }
}

/// HashMap values iterator.

pub struct Values<'a, K: 'a, V: 'a> {
    inner: Map<Iter<'a, K, V>, fn((&'a K, &'a V)) -> &'a V>
}

// FIXME(#19839) Remove in favor of `#[derive(Clone)]`

impl<'a, K, V> Clone for Values<'a, K, V> {
    fn clone(&self) -> Values<'a, K, V> {
        Values {
            inner: self.inner.clone()
        }
    }
}

/// HashMap drain iterator.

pub struct Drain<'a, K: 'a, V: 'a> {
    inner: iter::Map<raw_table::Drain<'a, K, V>, fn((SafeHash, K, V)) -> (K, V)>
}

/// A view into a single occupied location in a HashMap.

pub struct OccupiedEntry<'a, K: 'a, V: 'a> {
    elem: FullBucket<K, V, &'a mut RawTable<K, V>>,
}

/// A view into a single empty location in a HashMap.

pub struct VacantEntry<'a, K: 'a, V: 'a> {
    hash: SafeHash,
    key: K,
    elem: VacantEntryState<K, V, &'a mut RawTable<K, V>>,
}

/// A view into a single location in a map, which may be vacant or occupied.

pub enum Entry<'a, K: 'a, V: 'a> {
    /// An occupied Entry.

    Occupied(OccupiedEntry<'a, K, V>),

    /// A vacant Entry.

    Vacant(VacantEntry<'a, K, V>),
}

/// Possible states of a VacantEntry.
enum VacantEntryState<K, V, M> {
    /// The index is occupied, but the key to insert has precedence,
    /// and will kick the current one out on insertion.
    NeqElem(FullBucket<K, V, M>, usize),
    /// The index is genuinely vacant.
    NoElem(EmptyBucket<K, V, M>),
}


impl<'a, K, V, S> IntoIterator for &'a HashMap<K, V, S>
where K: Eq + Hash, S: HashState
{
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}


impl<'a, K, V, S> IntoIterator for &'a mut HashMap<K, V, S>
where K: Eq + Hash, S: HashState
{
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    fn into_iter(mut self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}


impl<K, V, S> IntoIterator for HashMap<K, V, S>
where K: Eq + Hash, S: HashState
{
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;

    /// Creates a consuming iterator, that is, one that moves each key-value
    /// pair out of the map in arbitrary order. The map cannot be used after
    /// calling this.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::HashMap;
    ///
    /// let mut map = HashMap::new();
    /// map.insert("a", 1);
    /// map.insert("b", 2);
    /// map.insert("c", 3);
    ///
    /// // Not possible with .iter()
    /// let vec: Vec<(&str, isize)> = map.into_iter().collect();
    /// ```
    fn into_iter(self) -> IntoIter<K, V> {
        fn last_two<A, B, C>((_, b, c): (A, B, C)) -> (B, C) { (b, c) }
        let last_two: fn((SafeHash, K, V)) -> (K, V) = last_two;

        IntoIter {
            inner: self.table.into_iter().map(last_two)
        }
    }
}


impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    #[inline] fn next(&mut self) -> Option<(&'a K, &'a V)> { self.inner.next() }
    #[inline] fn size_hint(&self) -> (usize, Option<usize>) { self.inner.size_hint() }
}

impl<'a, K, V> ExactSizeIterator for Iter<'a, K, V> {
    #[inline] fn len(&self) -> usize { self.inner.len() }
}


impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    #[inline] fn next(&mut self) -> Option<(&'a K, &'a mut V)> { self.inner.next() }
    #[inline] fn size_hint(&self) -> (usize, Option<usize>) { self.inner.size_hint() }
}

impl<'a, K, V> ExactSizeIterator for IterMut<'a, K, V> {
    #[inline] fn len(&self) -> usize { self.inner.len() }
}


impl<K, V> Iterator for IntoIter<K, V> {
    type Item = (K, V);

    #[inline] fn next(&mut self) -> Option<(K, V)> { self.inner.next() }
    #[inline] fn size_hint(&self) -> (usize, Option<usize>) { self.inner.size_hint() }
}

impl<K, V> ExactSizeIterator for IntoIter<K, V> {
    #[inline] fn len(&self) -> usize { self.inner.len() }
}


impl<'a, K, V> Iterator for Keys<'a, K, V> {
    type Item = &'a K;

    #[inline] fn next(&mut self) -> Option<(&'a K)> { self.inner.next() }
    #[inline] fn size_hint(&self) -> (usize, Option<usize>) { self.inner.size_hint() }
}

impl<'a, K, V> ExactSizeIterator for Keys<'a, K, V> {
    #[inline] fn len(&self) -> usize { self.inner.len() }
}


impl<'a, K, V> Iterator for Values<'a, K, V> {
    type Item = &'a V;

    #[inline] fn next(&mut self) -> Option<(&'a V)> { self.inner.next() }
    #[inline] fn size_hint(&self) -> (usize, Option<usize>) { self.inner.size_hint() }
}

impl<'a, K, V> ExactSizeIterator for Values<'a, K, V> {
    #[inline] fn len(&self) -> usize { self.inner.len() }
}


impl<'a, K, V> Iterator for Drain<'a, K, V> {
    type Item = (K, V);

    #[inline] fn next(&mut self) -> Option<(K, V)> { self.inner.next() }
    #[inline] fn size_hint(&self) -> (usize, Option<usize>) { self.inner.size_hint() }
}

impl<'a, K, V> ExactSizeIterator for Drain<'a, K, V> {
    #[inline] fn len(&self) -> usize { self.inner.len() }
}

impl<'a, K, V> Entry<'a, K, V> {

    /// Ensures a value is in the entry by inserting the default if empty, and returns
    /// a mutable reference to the value in the entry.
    pub fn or_insert(self, default: V) -> &'a mut V {
        match self {
            Occupied(entry) => entry.into_mut(),
            Vacant(entry) => entry.insert(default),
        }
    }


    /// Ensures a value is in the entry by inserting the result of the default function if empty,
    /// and returns a mutable reference to the value in the entry.
    pub fn or_insert_with<F: FnOnce() -> V>(self, default: F) -> &'a mut V {
        match self {
            Occupied(entry) => entry.into_mut(),
            Vacant(entry) => entry.insert(default()),
        }
    }
}

impl<'a, K, V> OccupiedEntry<'a, K, V> {
    /// Gets a reference to the value in the entry.

    pub fn get(&self) -> &V {
        self.elem.read().1
    }

    /// Gets a mutable reference to the value in the entry.

    pub fn get_mut(&mut self) -> &mut V {
        self.elem.read_mut().1
    }

    /// Converts the OccupiedEntry into a mutable reference to the value in the entry
    /// with a lifetime bound to the map itself

    pub fn into_mut(self) -> &'a mut V {
        self.elem.into_mut_refs().1
    }

    /// Sets the value of the entry, and returns the entry's old value

    pub fn insert(&mut self, mut value: V) -> V {
        let old_value = self.get_mut();
        mem::swap(&mut value, old_value);
        value
    }

    /// Takes the value out of the entry, and returns it

    pub fn remove(self) -> V {
        pop_internal(self.elem).1
    }
}

impl<'a, K: 'a, V: 'a> VacantEntry<'a, K, V> {
    /// Sets the value of the entry with the VacantEntry's key,
    /// and returns a mutable reference to it

    pub fn insert(self, value: V) -> &'a mut V {
        match self.elem {
            NeqElem(bucket, ib) => {
                robin_hood(bucket, ib, self.hash, self.key, value)
            }
            NoElem(bucket) => {
                bucket.put(self.hash, self.key, value).into_mut_refs().1
            }
        }
    }
}


impl<K, V, S> FromIterator<(K, V)> for HashMap<K, V, S>
where K: Eq + Hash, S: HashState + Default
{
    fn from_iter<T: IntoIterator<Item=(K, V)>>(iterable: T) -> HashMap<K, V, S> {
        let iter = iterable.into_iter();
        let lower = iter.size_hint().0;
        let mut map = HashMap::with_capacity_and_hash_state(lower,
                                                            Default::default());
        map.extend(iter);
        map
    }
}


impl<K, V, S> Extend<(K, V)> for HashMap<K, V, S>
where K: Eq + Hash, S: HashState
{
    fn extend<T: IntoIterator<Item=(K, V)>>(&mut self, iter: T) {
        for (k, v) in iter {
            self.insert(k, v);
        }
    }
}


impl<'a, K, V, S> Extend<(&'a K, &'a V)> for HashMap<K, V, S>
where K: Eq + Hash + Copy, V: Copy, S: HashState
{
    fn extend<T: IntoIterator<Item=(&'a K, &'a V)>>(&mut self, iter: T) {
        self.extend(iter.into_iter().map(|(&key, &value)| (key, value)));
    }
}

/// `RandomState` is the default state for `HashMap` types.
///
/// A particular instance `RandomState` will create the same instances of
/// `Hasher`, but the hashers created by two different `RandomState`
/// instances are unlikely to produce the same result for the same values.
#[derive(Clone)]

pub struct RandomState {
    k0: u64,
    k1: u64,
}


impl RandomState {
    /// Constructs a new `RandomState` that is initialized with random keys.
    #[inline]
    #[allow(deprecated)] // rand
    pub fn new() -> RandomState {
        use rand::XorShiftRng;
        let mut r = SeedableRng::from_seed([34330269583275, 23843948374, 3294234708234, 3214093874081234]): XorShiftRng;
        RandomState { k0: r.gen(), k1: r.gen() }
    }
}


impl HashState for RandomState {
    type Hasher = SipHasher;
    #[inline]
    fn hasher(&self) -> SipHasher {
        SipHasher::new_with_keys(self.k0, self.k1)
    }
}


impl Default for RandomState {
    #[inline]
    fn default() -> RandomState {
        RandomState::new()
    }
}

impl<K, S, Q: ?Sized> Recover<Q> for HashMap<K, (), S>
where K: Eq + Hash + Borrow<Q>, S: HashState, Q: Eq + Hash
{
    type Key = K;

    fn get(&self, key: &Q) -> Option<&K> {
        self.search(key).map(|bucket| bucket.into_refs().0)
    }

    fn take(&mut self, key: &Q) -> Option<K> {
        if self.table.size() == 0 {
            return None
        }

        self.search_mut(key).map(|bucket| pop_internal(bucket).0)
    }

    fn replace(&mut self, key: K) -> Option<K> {
        let hash = self.make_hash(&key);
        self.reserve(1);

        let mut retkey = None;
        self.insert_or_replace_with(hash, key, (), |key_ref, _, key, _| {
            retkey = Some(replace(key_ref, key));
        });
        retkey
    }
}

trait Recover<Q: ?Sized> {
    type Key;

    fn get(&self, key: &Q) -> Option<&Self::Key>;
    fn take(&mut self, key: &Q) -> Option<Self::Key>;
    fn replace(&mut self, key: Self::Key) -> Option<Self::Key>;
}

#[cfg(test)]
mod test_map {
    use prelude::v1::*;

    use super::HashMap;
    use super::Entry::{Occupied, Vacant};
    use cell::RefCell;
    use rand::{SeedableRng, Rng};

    #[test]
    fn test_create_capacity_zero() {
        let mut m = HashMap::with_capacity(0);

        assert!(m.insert(1, 1).is_none());

        assert!(m.contains_key(&1));
        assert!(!m.contains_key(&0));
    }

    #[test]
    fn test_insert() {
        let mut m = HashMap::new();
        assert_eq!(m.len(), 0);
        assert!(m.insert(1, 2).is_none());
        assert_eq!(m.len(), 1);
        assert!(m.insert(2, 4).is_none());
        assert_eq!(m.len(), 2);
        assert_eq!(*m.get(&1).unwrap(), 2);
        assert_eq!(*m.get(&2).unwrap(), 4);
    }

    thread_local! { static DROP_VECTOR: RefCell<Vec<isize>> = RefCell::new(Vec::new()) }

    #[derive(Hash, PartialEq, Eq)]
    struct Dropable {
        k: usize
    }

    impl Dropable {
        fn new(k: usize) -> Dropable {
            DROP_VECTOR.with(|slot| {
                slot.borrow_mut()[k] += 1;
            });

            Dropable { k: k }
        }
    }

    impl Drop for Dropable {
        fn drop(&mut self) {
            DROP_VECTOR.with(|slot| {
                slot.borrow_mut()[self.k] -= 1;
            });
        }
    }

    impl Clone for Dropable {
        fn clone(&self) -> Dropable {
            Dropable::new(self.k)
        }
    }

    #[test]
    fn test_drops() {
        DROP_VECTOR.with(|slot| {
            *slot.borrow_mut() = vec![0; 200];
        });

        {
            let mut m = HashMap::new();

            DROP_VECTOR.with(|v| {
                for i in 0..200 {
                    assert_eq!(v.borrow()[i], 0);
                }
            });

            for i in 0..100 {
                let d1 = Dropable::new(i);
                let d2 = Dropable::new(i+100);
                m.insert(d1, d2);
            }

            DROP_VECTOR.with(|v| {
                for i in 0..200 {
                    assert_eq!(v.borrow()[i], 1);
                }
            });

            for i in 0..50 {
                let k = Dropable::new(i);
                let v = m.remove(&k);

                assert!(v.is_some());

                DROP_VECTOR.with(|v| {
                    assert_eq!(v.borrow()[i], 1);
                    assert_eq!(v.borrow()[i+100], 1);
                });
            }

            DROP_VECTOR.with(|v| {
                for i in 0..50 {
                    assert_eq!(v.borrow()[i], 0);
                    assert_eq!(v.borrow()[i+100], 0);
                }

                for i in 50..100 {
                    assert_eq!(v.borrow()[i], 1);
                    assert_eq!(v.borrow()[i+100], 1);
                }
            });
        }

        DROP_VECTOR.with(|v| {
            for i in 0..200 {
                assert_eq!(v.borrow()[i], 0);
            }
        });
    }

    #[test]
    fn test_move_iter_drops() {
        DROP_VECTOR.with(|v| {
            *v.borrow_mut() = vec![0; 200];
        });

        let hm = {
            let mut hm = HashMap::new();

            DROP_VECTOR.with(|v| {
                for i in 0..200 {
                    assert_eq!(v.borrow()[i], 0);
                }
            });

            for i in 0..100 {
                let d1 = Dropable::new(i);
                let d2 = Dropable::new(i+100);
                hm.insert(d1, d2);
            }

            DROP_VECTOR.with(|v| {
                for i in 0..200 {
                    assert_eq!(v.borrow()[i], 1);
                }
            });

            hm
        };

        // By the way, ensure that cloning doesn't screw up the dropping.
        drop(hm.clone());

        {
            let mut half = hm.into_iter().take(50);

            DROP_VECTOR.with(|v| {
                for i in 0..200 {
                    assert_eq!(v.borrow()[i], 1);
                }
            });

            for _ in half.by_ref() {}

            DROP_VECTOR.with(|v| {
                let nk = (0..100).filter(|&i| {
                    v.borrow()[i] == 1
                }).count();

                let nv = (0..100).filter(|&i| {
                    v.borrow()[i+100] == 1
                }).count();

                assert_eq!(nk, 50);
                assert_eq!(nv, 50);
            });
        };

        DROP_VECTOR.with(|v| {
            for i in 0..200 {
                assert_eq!(v.borrow()[i], 0);
            }
        });
    }

    #[test]
    fn test_empty_pop() {
        let mut m: HashMap<isize, bool> = HashMap::new();
        assert_eq!(m.remove(&0), None);
    }

    #[test]
    fn test_lots_of_insertions() {
        let mut m = HashMap::new();

        // Try this a few times to make sure we never screw up the hashmap's
        // internal state.
        for _ in 0..10 {
            assert!(m.is_empty());

            for i in 1..1001 {
                assert!(m.insert(i, i).is_none());

                for j in 1..i+1 {
                    let r = m.get(&j);
                    assert_eq!(r, Some(&j));
                }

                for j in i+1..1001 {
                    let r = m.get(&j);
                    assert_eq!(r, None);
                }
            }

            for i in 1001..2001 {
                assert!(!m.contains_key(&i));
            }

            // remove forwards
            for i in 1..1001 {
                assert!(m.remove(&i).is_some());

                for j in 1..i+1 {
                    assert!(!m.contains_key(&j));
                }

                for j in i+1..1001 {
                    assert!(m.contains_key(&j));
                }
            }

            for i in 1..1001 {
                assert!(!m.contains_key(&i));
            }

            for i in 1..1001 {
                assert!(m.insert(i, i).is_none());
            }

            // remove backwards
            for i in (1..1001).rev() {
                assert!(m.remove(&i).is_some());

                for j in i..1001 {
                    assert!(!m.contains_key(&j));
                }

                for j in 1..i {
                    assert!(m.contains_key(&j));
                }
            }
        }
    }

    #[test]
    fn test_find_mut() {
        let mut m = HashMap::new();
        assert!(m.insert(1, 12).is_none());
        assert!(m.insert(2, 8).is_none());
        assert!(m.insert(5, 14).is_none());
        let new = 100;
        match m.get_mut(&5) {
            None => panic!(), Some(x) => *x = new
        }
        assert_eq!(m.get(&5), Some(&new));
    }

    #[test]
    fn test_insert_overwrite() {
        let mut m = HashMap::new();
        assert!(m.insert(1, 2).is_none());
        assert_eq!(*m.get(&1).unwrap(), 2);
        assert!(!m.insert(1, 3).is_none());
        assert_eq!(*m.get(&1).unwrap(), 3);
    }

    #[test]
    fn test_insert_conflicts() {
        let mut m = HashMap::with_capacity(4);
        assert!(m.insert(1, 2).is_none());
        assert!(m.insert(5, 3).is_none());
        assert!(m.insert(9, 4).is_none());
        assert_eq!(*m.get(&9).unwrap(), 4);
        assert_eq!(*m.get(&5).unwrap(), 3);
        assert_eq!(*m.get(&1).unwrap(), 2);
    }

    #[test]
    fn test_conflict_remove() {
        let mut m = HashMap::with_capacity(4);
        assert!(m.insert(1, 2).is_none());
        assert_eq!(*m.get(&1).unwrap(), 2);
        assert!(m.insert(5, 3).is_none());
        assert_eq!(*m.get(&1).unwrap(), 2);
        assert_eq!(*m.get(&5).unwrap(), 3);
        assert!(m.insert(9, 4).is_none());
        assert_eq!(*m.get(&1).unwrap(), 2);
        assert_eq!(*m.get(&5).unwrap(), 3);
        assert_eq!(*m.get(&9).unwrap(), 4);
        assert!(m.remove(&1).is_some());
        assert_eq!(*m.get(&9).unwrap(), 4);
        assert_eq!(*m.get(&5).unwrap(), 3);
    }

    #[test]
    fn test_is_empty() {
        let mut m = HashMap::with_capacity(4);
        assert!(m.insert(1, 2).is_none());
        assert!(!m.is_empty());
        assert!(m.remove(&1).is_some());
        assert!(m.is_empty());
    }

    #[test]
    fn test_pop() {
        let mut m = HashMap::new();
        m.insert(1, 2);
        assert_eq!(m.remove(&1), Some(2));
        assert_eq!(m.remove(&1), None);
    }

    #[test]
    fn test_iterate() {
        let mut m = HashMap::with_capacity(4);
        for i in 0..32 {
            assert!(m.insert(i, i*2).is_none());
        }
        assert_eq!(m.len(), 32);

        let mut observed: u32 = 0;

        for (k, v) in &m {
            assert_eq!(*v, *k * 2);
            observed |= 1 << *k;
        }
        assert_eq!(observed, 0xFFFF_FFFF);
    }

    #[test]
    fn test_keys() {
        let vec = vec![(1, 'a'), (2, 'b'), (3, 'c')];
        let map: HashMap<_, _> = vec.into_iter().collect();
        let keys: Vec<_> = map.keys().cloned().collect();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&1));
        assert!(keys.contains(&2));
        assert!(keys.contains(&3));
    }

    #[test]
    fn test_values() {
        let vec = vec![(1, 'a'), (2, 'b'), (3, 'c')];
        let map: HashMap<_, _> = vec.into_iter().collect();
        let values: Vec<_> = map.values().cloned().collect();
        assert_eq!(values.len(), 3);
        assert!(values.contains(&'a'));
        assert!(values.contains(&'b'));
        assert!(values.contains(&'c'));
    }

    #[test]
    fn test_find() {
        let mut m = HashMap::new();
        assert!(m.get(&1).is_none());
        m.insert(1, 2);
        match m.get(&1) {
            None => panic!(),
            Some(v) => assert_eq!(*v, 2)
        }
    }

    #[test]
    fn test_eq() {
        let mut m1 = HashMap::new();
        m1.insert(1, 2);
        m1.insert(2, 3);
        m1.insert(3, 4);

        let mut m2 = HashMap::new();
        m2.insert(1, 2);
        m2.insert(2, 3);

        assert!(m1 != m2);

        m2.insert(3, 4);

        assert_eq!(m1, m2);
    }

    #[test]
    fn test_show() {
        let mut map = HashMap::new();
        let empty: HashMap<i32, i32> = HashMap::new();

        map.insert(1, 2);
        map.insert(3, 4);

        let map_str = format!("{:?}", map);

        assert!(map_str == "{1: 2, 3: 4}" ||
                map_str == "{3: 4, 1: 2}");
        assert_eq!(format!("{:?}", empty), "{}");
    }

    #[test]
    fn test_expand() {
        let mut m = HashMap::new();

        assert_eq!(m.len(), 0);
        assert!(m.is_empty());

        let mut i = 0;
        let old_cap = m.table.capacity();
        while old_cap == m.table.capacity() {
            m.insert(i, i);
            i += 1;
        }

        assert_eq!(m.len(), i);
        assert!(!m.is_empty());
    }

    #[test]
    fn test_behavior_resize_policy() {
        let mut m = HashMap::new();

        assert_eq!(m.len(), 0);
        assert_eq!(m.table.capacity(), 0);
        assert!(m.is_empty());

        m.insert(0, 0);
        m.remove(&0);
        assert!(m.is_empty());
        let initial_cap = m.table.capacity();
        m.reserve(initial_cap);
        let cap = m.table.capacity();

        assert_eq!(cap, initial_cap * 2);

        let mut i = 0;
        for _ in 0..cap * 3 / 4 {
            m.insert(i, i);
            i += 1;
        }
        // three quarters full

        assert_eq!(m.len(), i);
        assert_eq!(m.table.capacity(), cap);

        for _ in 0..cap / 4 {
            m.insert(i, i);
            i += 1;
        }
        // half full

        let new_cap = m.table.capacity();
        assert_eq!(new_cap, cap * 2);

        for _ in 0..cap / 2 - 1 {
            i -= 1;
            m.remove(&i);
            assert_eq!(m.table.capacity(), new_cap);
        }
        // A little more than one quarter full.
        m.shrink_to_fit();
        assert_eq!(m.table.capacity(), cap);
        // again, a little more than half full
        for _ in 0..cap / 2 - 1 {
            i -= 1;
            m.remove(&i);
        }
        m.shrink_to_fit();

        assert_eq!(m.len(), i);
        assert!(!m.is_empty());
        assert_eq!(m.table.capacity(), initial_cap);
    }

    #[test]
    fn test_reserve_shrink_to_fit() {
        let mut m = HashMap::new();
        m.insert(0, 0);
        m.remove(&0);
        assert!(m.capacity() >= m.len());
        for i in 0..128 {
            m.insert(i, i);
        }
        m.reserve(256);

        let usable_cap = m.capacity();
        for i in 128..(128 + 256) {
            m.insert(i, i);
            assert_eq!(m.capacity(), usable_cap);
        }

        for i in 100..(128 + 256) {
            assert_eq!(m.remove(&i), Some(i));
        }
        m.shrink_to_fit();

        assert_eq!(m.len(), 100);
        assert!(!m.is_empty());
        assert!(m.capacity() >= m.len());

        for i in 0..100 {
            assert_eq!(m.remove(&i), Some(i));
        }
        m.shrink_to_fit();
        m.insert(0, 0);

        assert_eq!(m.len(), 1);
        assert!(m.capacity() >= m.len());
        assert_eq!(m.remove(&0), Some(0));
    }

    #[test]
    fn test_from_iter() {
        let xs = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)];

        let map: HashMap<_, _> = xs.iter().cloned().collect();

        for &(k, v) in &xs {
            assert_eq!(map.get(&k), Some(&v));
        }
    }

    #[test]
    fn test_size_hint() {
        let xs = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)];

        let map: HashMap<_, _>  = xs.iter().cloned().collect();

        let mut iter = map.iter();

        for _ in iter.by_ref().take(3) {}

        assert_eq!(iter.size_hint(), (3, Some(3)));
    }

    #[test]
    fn test_iter_len() {
        let xs = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)];

        let map: HashMap<_, _>  = xs.iter().cloned().collect();

        let mut iter = map.iter();

        for _ in iter.by_ref().take(3) {}

        assert_eq!(iter.len(), 3);
    }

    #[test]
    fn test_mut_size_hint() {
        let xs = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)];

        let mut map: HashMap<_, _>  = xs.iter().cloned().collect();

        let mut iter = map.iter_mut();

        for _ in iter.by_ref().take(3) {}

        assert_eq!(iter.size_hint(), (3, Some(3)));
    }

    #[test]
    fn test_iter_mut_len() {
        let xs = [(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (6, 6)];

        let mut map: HashMap<_, _>  = xs.iter().cloned().collect();

        let mut iter = map.iter_mut();

        for _ in iter.by_ref().take(3) {}

        assert_eq!(iter.len(), 3);
    }

    #[test]
    fn test_index() {
        let mut map = HashMap::new();

        map.insert(1, 2);
        map.insert(2, 1);
        map.insert(3, 4);

        assert_eq!(map[&2], 1);
    }

    #[test]
    #[should_panic]
    fn test_index_nonexistent() {
        let mut map = HashMap::new();

        map.insert(1, 2);
        map.insert(2, 1);
        map.insert(3, 4);

        map[&4];
    }

    #[test]
    fn test_entry(){
        let xs = [(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)];

        let mut map: HashMap<_, _> = xs.iter().cloned().collect();

        // Existing key (insert)
        match map.entry(1) {
            Vacant(_) => unreachable!(),
            Occupied(mut view) => {
                assert_eq!(view.get(), &10);
                assert_eq!(view.insert(100), 10);
            }
        }
        assert_eq!(map.get(&1).unwrap(), &100);
        assert_eq!(map.len(), 6);


        // Existing key (update)
        match map.entry(2) {
            Vacant(_) => unreachable!(),
            Occupied(mut view) => {
                let v = view.get_mut();
                let new_v = (*v) * 10;
                *v = new_v;
            }
        }
        assert_eq!(map.get(&2).unwrap(), &200);
        assert_eq!(map.len(), 6);

        // Existing key (take)
        match map.entry(3) {
            Vacant(_) => unreachable!(),
            Occupied(view) => {
                assert_eq!(view.remove(), 30);
            }
        }
        assert_eq!(map.get(&3), None);
        assert_eq!(map.len(), 5);


        // Inexistent key (insert)
        match map.entry(10) {
            Occupied(_) => unreachable!(),
            Vacant(view) => {
                assert_eq!(*view.insert(1000), 1000);
            }
        }
        assert_eq!(map.get(&10).unwrap(), &1000);
        assert_eq!(map.len(), 6);
    }

    #[test]
    fn test_entry_take_doesnt_corrupt() {
        #![allow(deprecated)] //rand
        // Test for #19292
        fn check(m: &HashMap<isize, ()>) {
            for k in m.keys() {
                assert!(m.contains_key(k),
                "{} is in keys() but not in the map?", k);
            }
        }

        let mut m = HashMap::new();
        let mut rng = SeedableRng::from_seed(912047394837942734);

        // Populate the map with some items.
        for _ in 0..50 {
            let x = rng.gen_range(-10, 10);
            m.insert(x, ());
        }

        for i in 0..1000 {
            let x = rng.gen_range(-10, 10);
            match m.entry(x) {
                Vacant(_) => {},
                Occupied(e) => {
                    println!("{}: remove {}", i, x);
                    e.remove();
                },
            }

            check(&m);
        }
    }

    #[test]
    fn test_extend_ref() {
        let mut a = HashMap::new();
        a.insert(1, "one");
        let mut b = HashMap::new();
        b.insert(2, "two");
        b.insert(3, "three");

        a.extend(&b);

        assert_eq!(a.len(), 3);
        assert_eq!(a[&1], "one");
        assert_eq!(a[&2], "two");
        assert_eq!(a[&3], "three");
    }
}
