//! Page management.
//!
//! Pages are virtual data units of size 4088 bytes. They're represented on disk somewhat
//! non-obviously, since clusters can hold more than one page at once (compression). Every cluster
//! will maximize the number of pages held and when it's filled up, a new cluster will be fetched.

/// The size (in bytes) of the metacluster header.
const METACLUSTER_HEADER: usize = 8;
/// The size (in bytes) of the metacluster's non-header section.
const METACLUSTER_SIZE: usize = disk::SECTOR - METACLUSTER_HEADER;
/// The size (in bytes) of the data cluster header.
const DATA_CLUSTER_HEADER: usize = 2;
/// The size (in bytes) of the data cluster's non-header section.
const DATA_CLUSTER_SIZE: usize = disk::SECTOR - DATA_CLUSTER_HEADER;

quick_error! {
    /// A page management error.
    enum Error {
        /// No clusters left in the freelist.
        ///
        /// This is the equivalent to OOM, but with disk space.
        OutOfClusters {
            description("Out of free clusters.")
        }
        /// The checksum of the data and the provided checksum does not match.
        ///
        /// This indicates some form of data corruption.
        ChecksumMismatch {
            cluster: cluster::Pointer,
            /// The checksum of the data.
            expected: u64,
            /// The expected/stored value of the checksum.
            found: u64,
        } {
            display("Mismatching checksums in cluster {} - expected {:x}, found {:x}.", cluster, expected, found)
            description("Mismatching checksum.")
        }
        /// The compressed data is invalid and cannot be decompressed.
        ///
        /// Multiple reasons exists for this to happen:
        ///
        /// 1. The compression configuration option has been changed without recompressing clusters.
        /// 2. Silent data corruption occured, and did the unlikely thing to has the right checksum.
        /// 3. There is a bug in compression or decompression.
        InvalidCompression {
            cluster: cluster::Pointer,
        } {
            display("Unable to decompress data from cluster {}.", cluster)
            description("Unable to decompress data.")
        }
        /// A disk error.
        Disk(err: disk::Error) {
            from()
            description("Disk I/O error")
            display("Disk I/O error: {}", err)
        }
    }
}

/// A state of a page manager.
struct State {
    /// The state block.
    ///
    /// The state block stores the state of the file system including allocation state,
    /// configuration, and more.
    state_block: state_block::StateBlock,
    /// The first chunk of the freelist.
    ///
    /// This list is used as the allocation primitive of TFS. It is a simple freelist-based extent
    /// allocation system, but there is one twist: To optimize the data locality, the list is
    /// unrolled.
    ///
    /// The first element (if any) points to _another_ freelist chunk (a "metacluster"), which can
    /// be used to traverse to the next metacluster when needed.
    freelist: Vec<cluster::Pointer>,
    /// The last allocated cluster.
    last_cluster: cluster::Pointer,
    /// The last allocated cluster's data decompressed.
    ///
    /// This is used for packing pages into the cluster, by appending the new page to this vector
    /// and then compressing it to see if it fits into the cluster. If it fails to fit, the vector
    /// is reset and a new cluster is allocated.
    last_cluster_data: Vec<u8>,
}

/// The page manager.
///
/// This is the center point of the I/O stack, providing allocation, deallocation, compression,
/// etc. It manages the clusters (with the page abstraction) and caches the disks.
struct Manager<D> {
    /// The inner disk.
    disk: Cache<header::Driver<D>>,
    /// The state of the manager.
    state: State,
    /// The state of the manager on the time of last cache commit.
    ///
    /// This contains the state of the page manager upon the last cache commit (pipeline flush). It
    /// is used to roll back the page manager when an error occurs.
    committed_state: State,
}

impl<D: Disk> Manager<D> {
    /// Commit the transactions in the pipeline to the cache.
    ///
    /// This runs over the transactions in the pipeline and applies them to the cache. In a sense,
    /// it can be seen as a form of checkpoint as you can revert to the last commit through
    /// `.revert()`, as it stores the old state.
    fn commit(&mut self) {
        // Update the stored committed state to the current state, which we will commit.
        self.committed_state = self.state.clone();
        // Commit the cache pipeline.
        self.disk.commit();
    }

    /// Revert to the last commit.
    ///
    /// This will reset the state to after the previous cache commit.
    fn revert(&mut self) {
        // Revert the state to when it was committed last time.
        self.state = self.committed_state.clone();
        // Revert the cache pipeline.
        self.disk.revert();
    }

    /// Queue a page allocation.
    ///
    /// This adds a transaction to the cache pipeline to allocate a page. It can be committed
    /// through `.commit()`.
    fn queue_alloc(&mut self, buf: &[u8]) -> Result<Pointer, Error> {
        // Allocate a buffer for constructing the cluster.
        let mut cluster = vec![0; DATA_CLUSTER_HEADER];
        // Extend the last allocated cluster with the new page.
        self.state.last_cluster_data.extend_from_slice(buf);
        // Compress the last allocated cluster.
        self.compress(self.state.last_cluster_data, &mut cluster);

        if cluster.len() <= disk::SECTOR_SIZE {
            // The pages could fit in the cluster.

            // Pad with zeros until the sector is full.
            while cluster.len() != disk::SECTOR_SIZE {
                cluster.push(0);
            }

            // Calculate and write the checksum.
            LittleEndian::write(&mut cluster, self.checksum(cluster[DATA_CLUSTER_HEADER..]) as u16);
            // Set the compression flag in the checksum field.
            cluster[1] <<= 1;
            cluster[1] |= 1;

            // Queue the write of the recompress cluster.
            self.state.queue(self.state.last_cluster, cluster.into_boxed_slice());
        } else {
            // Unable to fit the pages into the cluster.

            // Truncate the unusable compressed buffer.
            cluster.truncate(DATA_CLUSTER_HEADER);
            // Extend the cluster with the buffer to allocate.
            cluster.extend_from_slice(&buf);

            // Calculate and write the checksum.
            LittleEndian::write(&mut cluster, self.checksum(cluster[DATA_CLUSTER_HEADER..]) as u16);
            // Set the compression flag in the checksum field to zero (i.e. uncompressed).
            cluster[1] <<= 1;

            // We cannot fit more into the last allocated cluster, so we clear it.
            self.state.last_cluster_data.clear();
            // Update it with the new given data.
            self.state.last_cluster_data.extend_from_slice(&buf);

            // Pop from the freelist and set this as the new last allocated cluster.
            self.state.last_cluster = self.queue_freelist_pop()?;

            // Queue a write to the new cluster.
            self.disk.queue(self.state.last_cluster, cluster);
        }
    }

    /// Calculate the checksum of some buffer, based on the user configuration.
    fn checksum(&self, buf: &[u8]) -> u64 {
        self.state.state_block.checksum_algorithm.hash(buf)
    }

    /// Compress some data based on the compression configuration option.
    ///
    /// This compresses `source` into `target` based on the chosen configuration method, defined in
    /// the state block.
    fn compress(&self, source: &[u8], target: &mut Vec<u8>) {
        match self.state.state_block.compression_algorithm {
            // Memcpy as a compression algorithm!!!11!
            CompressionAlgorithm::Identity => target.extend_from_slice(source),
            // Compress via LZ4.
            CompressionAlgorithm::Lz4 => lz4_compress::compress_into(source, target),
        }
    }

    /// Decompress some data based on the compression configuration option.
    ///
    /// This decompresses `source` into `target` based on the chosen configuration method, defined
    /// in the state block.
    fn decompress(&self, source: &[u8], target: &mut Vec<u8>) -> Result<(), Error> {
        match self.state.state_block.compression_algorithm {
            // Memcpy as a compression algorithm!!!11!
            CompressionAlgorithm::Identity => target.extend_from_slice(source),
            // Decompress from LZ4.
            CompressionAlgorithm::Lz4 => lz4_compress::decompress_from(source, target)?,
        }

        Ok(())
    }

    /// Queue a state block flush.
    ///
    /// This queues a new transaction flushing the state block.
    fn queue_state_block_flush(&mut self) {
        self.disk.queue(self.header.state_block_address, self.state.state_block.into());
    }

    /// Queue a freelist head flush.
    ///
    /// This queues a new transaction flushing the freelist head.
    fn queue_freelist_head_flush(&mut self) {
        // Start with an all-null cluster buffer.
        let mut buf = Box::new([0; disk::SECTOR_SIZE]);

        // Write every pointer of the freelist into the buffer.
        for (n, i) in self.free.iter().enumerate() {
            LittleEndian::write(&mut buf[cluster::POINTER_SIZE * i + METACLUSTER_HEADER..], i);
        }

        // Checksum the non-checksum part of the buffer, and write it at the start of the buffer.
        LittleEndian::write(&mut buf, self.checksum(&buf[2..]));

        // Queue the write of the updated buffer.
        self.disk.queue(self.state.state_block.freelist_head, buf);
    }

    /// Queue a pop from the freelist.
    ///
    /// This adds a new transaction to the cache pipeline, which will pop from the top of the
    /// freelist and return the result.
    fn queue_freelist_pop(&mut self) -> Result<cluster::Pointer, Error> {
        // Pop from the metacluster.
        if let Some(cluster) = self.state.freelist.pop() {
            if self.freelist.head.free.is_empty() {
                // The head metacluster is exhausted, so we load the next metacluster (specified to be
                // the last pointer in the metacluster), i.e. `cluster`. The old metacluster is then
                // used as the popped cluster.
                mem::swap(&mut self.state.state_block.cluster, &mut cluster);
                self.state.load_freelist(self.disk.read(self.state_block.freelist_head)?);

                // We've updated the state block, so we queue a flush to the disk.
                self.queue_state_block_flush();
            } else {
                // Since the freelist head was changed after the pop, we queue a flush.
                self.queue_freelist_head_flush();
            }

            Ok(cluster)
        } else {
            // We ran out of clusters :(.
            Err(Error::OutOfClusters)
        }
    }

    /// Queue a push to the freelist.
    ///
    /// This adds a new transaction to the cache pipeline, which will push some free cluster to the
    /// top of the freelist.
    fn queue_freelist_push(&mut self, cluster: cluster::Pointer) -> Result<(), Error> {
        // If enabled, purge the data of the cluster.
        if cfg!(feature = "security") {
            self.disk.queue(cluster, vec![0; disk::SECTOR_SIZE].into_boxed_slice());
        }

        if self.state.freelist.len() == METACLUSTER_SIZE / cluster::POINTER_SIZE {
            // The freelist head is full, and therefore we use following algorithm:
            //
            // 1. Create a new metacluster at `cluster`.
            // 2. Link said metacluster to the old metacluster.
            // 3. Queue a flush.

            // Clear the in-memory freelist head mirror.
            self.state.freelist.clear();
            // Put the link to the old freelist head into the new metacluster.
            self.state.freelist.push(state_block.freelist_head);

            // Update the freelist head pointer to point to the new metacluster.
            self.state.state_block.freelist_head = cluster;
            // Queue a flush of the new freelist head. This won't leave the system in an
            // inconsistent state as it merely creates a new metacluster, which is first linked
            // later. If the state block flush fails, the metacluster will merely be an orphan
            // cluster, and therefore simply leaked space.
            self.queue_freelist_head_flush();
            // Queue a flush of the state block (or, in particular, the freelist head pointer).
            // This is completely consistent as the freelist head must flush before, thus rendering
            // the pointed cluster a valid metacluster.
            self.queue_state_block_flush();
        } else {
            // There is space for more clusters in the head metacluster.

            // Push the cluster pointer to the freelist head.
            self.state.freelist.push(cluster);
            // Queue a flush of the new freelist head.
            self.queue_freelist_head_flush();

            // lulz @ these comments. like shit, ticki, they add basically nothing you fuking dumb
            // monkey. seriously stop it
        }
    }
}
