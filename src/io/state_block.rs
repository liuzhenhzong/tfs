quick_error! {
    /// A state block parsing error.
    enum Error {
        /// Unknown or implementation-specific compression algorithm.
        UnknownCompressionAlgorithm {
            description("Unknown compression algorithm option.")
        }
        /// Invalid compression algorithm.
        InvalidCompressionAlgorithm {
            description("Invalid compression algorithm option.")
        }
        /// The checksums doesn't match.
        ChecksumMismatch {
            /// The checksum of the data.
            expected: u16,
            /// The expected/stored value of the checksum.
            found: u16,
        } {
            display("Mismatching checksums in the state block - expected {:x}, found {:x}.", expected, found)
            description("Mismatching checksum.")
        }
    }
}

/// A compression algorithm configuration option.
enum CompressionAlgorithm {
    /// Identity function/compression disabled.
    Identity = 0,
    /// LZ4 compression.
    ///
    /// LZ4 is a very fast LZ77-family compression algorithm. Like other LZ77 compressors, it is
    /// based on streaming data reduplication. The details are described
    /// [here](http://ticki.github.io/blog/how-lz4-works/).
    Lz4 = 1,
}

impl TryFrom<u16> for CompressionAlgorithm {
    type Err = Error;

    fn try_from(from: u16) -> Result<CompressionAlgorithm, Error> {
        match from {
            0 => Ok(CompressionAlgorithm::Identity),
            1 => Ok(CompressionAlgorithm::Lz4),
            1 << 15... => Err(Error::UnknownCompressionAlgorithm),
            _ => Err(Error::InvalidCompressionAlgorithm),
        }
    }
}

/// The TFS state block.
struct StateBlock {
    /// The chosen compression algorithm.
    compression_algorithm: CompressionAlgorithm,
    /// A pointer to the head of the freelist.
    freelist_head: cluster::Pointer,
    /// A pointer to the superpage.
    superpage: pages::Pointer,
}

impl StateBlock {
    /// Parse a sequence of bytes.
    fn decode(buf: &[u8], checksum_algorithm: header::ChecksumAlgorithm) -> Result<(), Error> {
        // Make sure that the checksum of the state block matches the 8 byte field in the start.
        let expected = LittleEndian::read(&buf);
        let found = checksum_algorithm.hash(&buf[8..]);
        if expected != found {
            return Err(Error::ChecksumMismatch {
                expected: expected,
                found: found,
            });
        }

        StateBlock {
            // Load the compression algorithm config field.
            compression_algorithm: CompressionAlgorithm::try_from(LittleEndian::read(buf[8..]))?,
            // Load the freelist head pointer.
            freelist_head: LittleEndian::read(buf[16..]),
            // Load the superpage pointer.
            superpage: LittleEndian::read(buf[24..]),
        }
    }

    /// Encode the state block into a sector-sized buffer.
    fn encode(&self, checksum_algorithm: header::ChecksumAlgorithm) -> [u8; disk::SECTOR_SIZE] {
        // Create a buffer to hold the data.
        let mut buf = [0; disk::SECTOR_SIZE];

        // Write the compression algorithm.
        LittleEndian::write(&mut buf[8..], self.compression_algorithm as u16);
        // Write the freelist head pointer.
        LittleEndian::write(&mut buf[16..], self.freelist_head);
        // Write the superpage pointer.
        LittleEndian::write(&mut buf[24..], self.superpage);

        // Calculate and store the checksum.
        let cksum = self.checksum_algorithm.hash(&buf[8..]);
        LittleEndian::write(&mut buf, cksum);

        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_identity() {
        let mut block = StateBlock::default();
        assert_eq!(StateBlock::decode(block.encode()).unwrap(), block);

        block.compression_algorithm = CompressionAlgorithm::Identity;
        assert_eq!(StateBlock::decode(block.encode()).unwrap(), block);

        block.freelist_head = 2000;
        assert_eq!(StateBlock::decode(block.encode()).unwrap(), block);

        block.superpage = 200;
        assert_eq!(StateBlock::decode(block.encode()).unwrap(), block);
    }

    #[test]
    fn manual_mutation() {
        let mut block = StateBlock::default();
        let mut sector = block.encode();

        block.compression_algorithm = CompressionAlgorithm::Identity;
        sector[9] = 0;
        LittleEndian::write(&mut sector, seahash::hash(sector[8..]));
        assert_eq!(sector, block.encode());

        block.freelist_head = 52;
        sector[16] = 52;
        LittleEndian::write(&mut sector, seahash::hash(sector[8..]));
        assert_eq!(sector, block.encode());

        block.superpage = 29;
        sector[24] = 29;
        LittleEndian::write(&mut sector, seahash::hash(sector[8..]));
        assert_eq!(sector, block.encode());
    }

    #[test]
    fn mismatching_checksum() {
        let mut sector = StateBlock::default().encode();
        sector[2] = 20;
        assert_eq!(StateBlock::decode(sector), Err(Error::ChecksumMismatch));
    }

    #[test]
    fn unknown_invalid_options() {
        let mut sector = StateBlock::default().encode();

        sector = StateBlock::default().encode();

        sector[8] = 0xFF;
        assert_eq!(StateBlock::decode(sector), Err(Error::InvalidCompression));
        sector[9] = 0xFF;
        assert_eq!(StateBlock::decode(sector), Err(Error::UnknownChecksumAlgorithm));
    }
}
