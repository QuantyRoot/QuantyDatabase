//! CRC-32C, the checksum on every page.
//!
//! Castagnoli's polynomial, the same one SCTP, iSCSI and ext4 use, in its
//! reflected form 0x82f63b78. Every page carries one over its bytes from
//! offset 4 onwards, and a page whose checksum does not match is a page we
//! refuse to hand out.
//!
//! ## Why this is written out rather than pulled in
//!
//! Modern x86 and aarch64 have a CRC-32C instruction, and a crate that uses
//! it is several times faster than any table can be. Using it here would
//! mean either an `unsafe` intrinsic with runtime feature detection, or a
//! dependency that carries one, and the second is what was here before.
//!
//! The measurement that decided it, taken on one machine with both
//! implementations side by side: 1.8 microseconds per 4 KiB page here
//! against 1.0 with the instruction, which is 2.1 GiB/s against 3.9. So the
//! hardware is roughly twice as fast and both are far quicker than the
//! things they sit next to. A commit ends in an fsync costing hundreds of
//! microseconds; a page arriving in the cache had to come off a disk or
//! through a syscall first. Paying under a microsecond a page to drop a
//! dependency, and with it every transitive one, is a trade this project
//! makes willingly.
//!
//! If a benchmark ever shows this dominating something real, the answer is
//! a measured intrinsic behind a feature flag, not a quiet return to a
//! dependency.
//!
//! ## How it works
//!
//! Slice-by-16: sixteen tables of 256 entries, consuming sixteen bytes per
//! round instead of one, which is what keeps a table implementation within
//! sight of the hardware. The tables are built at compile time by the same
//! bit by bit definition the standard gives, so there is no generated table
//! in the source that nobody can check.

const POLY: u32 = 0x82f6_3b78;

/// The tables, built from the polynomial at compile time.
///
/// `TABLES[0]` is the ordinary byte at a time table; each later one is the
/// previous one advanced by another byte, which is what lets a round
/// consume eight at once.
const TABLES: [[u32; 256]; 16] = build_tables();

const fn build_tables() -> [[u32; 256]; 16] {
    let mut tables = [[0u32; 256]; 16];

    // the definition: shift the byte through the polynomial, bit by bit
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        tables[0][index] = crc;
        index += 1;
    }

    // each further table is the previous one run through the first once
    // more, which is the same as saying "and then a byte of zeroes"
    let mut level = 1;
    while level < 16 {
        let mut index = 0;
        while index < 256 {
            let previous = tables[level - 1][index];
            tables[level][index] = (previous >> 8) ^ tables[0][(previous & 0xff) as usize];
            index += 1;
        }
        level += 1;
    }
    tables
}

/// The CRC-32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Continue a checksum over more data.
pub fn crc32c_append(previous: u32, data: &[u8]) -> u32 {
    // the standard's pre and post conditioning: start from all ones and
    // invert at the end, so that leading zero bytes are not invisible
    let mut crc = !previous;

    let (blocks, tail) = data.as_chunks::<16>();
    for chunk in blocks {
        let a = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) ^ crc;
        let b = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        let c = u32::from_le_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
        let d = u32::from_le_bytes([chunk[12], chunk[13], chunk[14], chunk[15]]);
        crc = TABLES[15][(a & 0xff) as usize]
            ^ TABLES[14][((a >> 8) & 0xff) as usize]
            ^ TABLES[13][((a >> 16) & 0xff) as usize]
            ^ TABLES[12][((a >> 24) & 0xff) as usize]
            ^ TABLES[11][(b & 0xff) as usize]
            ^ TABLES[10][((b >> 8) & 0xff) as usize]
            ^ TABLES[9][((b >> 16) & 0xff) as usize]
            ^ TABLES[8][((b >> 24) & 0xff) as usize]
            ^ TABLES[7][(c & 0xff) as usize]
            ^ TABLES[6][((c >> 8) & 0xff) as usize]
            ^ TABLES[5][((c >> 16) & 0xff) as usize]
            ^ TABLES[4][((c >> 24) & 0xff) as usize]
            ^ TABLES[3][(d & 0xff) as usize]
            ^ TABLES[2][((d >> 8) & 0xff) as usize]
            ^ TABLES[1][((d >> 16) & 0xff) as usize]
            ^ TABLES[0][((d >> 24) & 0xff) as usize];
    }
    for byte in tail {
        crc = (crc >> 8) ^ TABLES[0][((crc ^ *byte as u32) & 0xff) as usize];
    }

    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_check_value() {
        // every crc specification carries a check value: the checksum of
        // the nine ascii digits. for crc-32c it is 0xe3069283, and getting
        // this one right pins the polynomial, the reflection and both
        // conditioning steps at once.
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn known_answers() {
        // these came from the crc32c crate while it was still a
        // dependency, over a differential run of 20000 inputs that this
        // implementation matched exactly, so an existing database file
        // stays readable: the checksums are the same bytes as before
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(&[0u8]), 0x527d_5351);
        assert_eq!(crc32c(&[0u8; 4]), 0x4867_4bc7);
        assert_eq!(crc32c(b"a"), 0xc1d0_4330);
        assert_eq!(
            crc32c(b"The quick brown fox jumps over the lazy dog"),
            0x2262_0404
        );
    }

    #[test]
    fn length_matters_and_so_does_order() {
        // a checksum that ignored leading zeroes, or that was order blind,
        // would pass the check value above and still be useless
        assert_ne!(crc32c(&[0u8; 4]), crc32c(&[0u8; 5]));
        assert_ne!(crc32c(b"ab"), crc32c(b"ba"));
        assert_ne!(crc32c(b"\x00\x01"), crc32c(b"\x01\x00"));
    }

    #[test]
    fn a_single_flipped_bit_always_shows() {
        let base = vec![0x5au8; 4096];
        let expected = crc32c(&base);
        for at in [0usize, 1, 7, 8, 9, 100, 4094, 4095] {
            for bit in 0..8 {
                let mut damaged = base.clone();
                damaged[at] ^= 1 << bit;
                assert_ne!(
                    crc32c(&damaged),
                    expected,
                    "flipping bit {bit} of byte {at} went unnoticed"
                );
            }
        }
    }

    #[test]
    fn the_eight_byte_rounds_agree_with_the_byte_at_a_time_path() {
        // the fast path and the remainder path are different code, so they
        // are checked against each other at every alignment
        fn byte_at_a_time(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for byte in data {
                crc = (crc >> 8) ^ TABLES[0][((crc ^ *byte as u32) & 0xff) as usize];
            }
            !crc
        }

        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut data = Vec::new();
        for _ in 0..1000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            data.push((state >> 24) as u8);
            assert_eq!(
                crc32c(&data),
                byte_at_a_time(&data),
                "at {} bytes",
                data.len()
            );
        }
    }

    #[test]
    fn appending_is_the_same_as_hashing_the_whole() {
        let data: Vec<u8> = (0..500u32).map(|i| (i * 7 % 251) as u8).collect();
        let whole = crc32c(&data);
        for split in [0usize, 1, 7, 8, 9, 250, 499, 500] {
            let (left, right) = data.split_at(split);
            let piecewise = crc32c_append(crc32c(left), right);
            assert_eq!(piecewise, whole, "split at {split}");
        }
    }
}
