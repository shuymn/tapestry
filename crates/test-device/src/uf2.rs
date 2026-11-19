//! The UF2 writer for the RP2350's boot ROM drive (docs/test-device.md
//! "Building"): 512-byte blocks of 256 payload bytes, family
//! `rp2350-arm-s`, written only for an image whose image definition lies
//! where the boot ROM looks for it; and the check ([`check`]) the host runs
//! on a UF2 before it reboots the device to copy one.

/// UF2's first and second magic words and its final magic word.
const MAGIC: [u32; 3] = [0x0A32_4655, 0x9E5D_5157, 0x0AB1_6F30];
/// The byte offsets of the three magic words in a block.
#[cfg(feature = "std")]
const MAGIC_AT: [usize; 3] = [0, 4, 508];
/// Bytes per UF2 block.
pub const BLOCK: usize = 512;
/// The block carries a family id.
const FLAG_FAMILY: u32 = 0x0000_2000;
/// The block's content is not for the device (`UF2_FLAG_NOT_MAIN_FLASH`,
/// the UF2 specification, github.com/microsoft/uf2, README "Flags").
#[cfg(feature = "std")]
const FLAG_NOT_MAIN_FLASH: u32 = 0x0000_0001;
/// The `rp2350-arm-s` family id.
pub const FAMILY_RP2350_ARM_S: u32 = 0xE48B_FF59;
/// The flash's address.
pub const FLASH_BASE: u32 = 0x1000_0000;
/// The Pico 2's flash size.
pub const FLASH_SIZE: u32 = 4 << 20;
/// Payload bytes per block, and the alignment of a block's flash address:
/// the only payload size the boot ROM takes (RP2350 datasheet, 5.5.2).
pub const PAGE: usize = 256;
/// The span the boot ROM searches for an image definition.
pub const SEARCH: usize = 4096;

const BLOCK_START: u32 = 0xFFFF_DED3;
const BLOCK_END: u32 = 0xAB12_3579;
/// The first item of an image definition: its image type.
const ITEM_IMAGE_TYPE: u8 = 0x42;

fn word(image: &[u8], at: usize) -> Option<u32> {
    let b = image.get(at..at + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Where the image definition starts: a block start marker at a word
/// boundary in the first [`SEARCH`] bytes, whose first item is an image
/// type, with its end marker after it in that span.
#[must_use]
pub fn image_def_offset(image: &[u8]) -> Option<usize> {
    let head = image.get(..SEARCH.min(image.len()))?;
    (0..head.len()).step_by(4).find(|&at| {
        word(head, at) == Some(BLOCK_START)
            && head.get(at + 4) == Some(&ITEM_IMAGE_TYPE)
            && (at + 8..head.len())
                .step_by(4)
                .any(|e| word(head, e) == Some(BLOCK_END))
    })
}

/// UF2 block `index` of `total`, carrying `payload` (at most [`PAGE`]
/// bytes, zero-padded) for flash address `addr`.
#[must_use]
pub fn block(index: u32, total: u32, addr: u32, payload: &[u8]) -> [u8; BLOCK] {
    let mut out = [0_u8; BLOCK];
    let head = [
        MAGIC[0],
        MAGIC[1],
        FLAG_FAMILY,
        addr,
        256,
        index,
        total,
        FAMILY_RP2350_ARM_S,
    ];
    for (dst, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(head) {
        dst.copy_from_slice(&w.to_le_bytes());
    }
    let n = payload.len().min(PAGE);
    if let (Some(dst), Some(src)) = (out.get_mut(32..32 + n), payload.get(..n)) {
        dst.copy_from_slice(src);
    }
    if let Some(dst) = out.get_mut(508..) {
        dst.copy_from_slice(&MAGIC[2].to_le_bytes());
    }
    out
}

/// A UF2 rule a file breaks; `block` is the block's position in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Uf2Error {
    /// The file has no bytes.
    Empty,
    /// The file's length is not a whole number of blocks.
    Length(usize),
    /// The magic word at byte `at` of the block (0, 4 or 508) is `found`.
    Magic { block: usize, at: usize, found: u32 },
    /// The block lacks the family-id flag.
    NoFamily { block: usize },
    /// The block names family `found`, not `rp2350-arm-s`.
    Family { block: usize, found: u32 },
    /// The block is marked not for the device (`UF2_FLAG_NOT_MAIN_FLASH`).
    NotMainFlash { block: usize },
    /// The block's payload is `size` bytes, not [`PAGE`].
    PayloadSize { block: usize, size: u32 },
    /// The block's flash address `addr` is not a multiple of [`PAGE`].
    Unaligned { block: usize, addr: u32 },
    /// The block writes `addr` onwards for `size` bytes, outside the flash.
    Address { block: usize, addr: u32, size: u32 },
    /// The block says the file has `says` blocks; it has `has`.
    NumBlocks { block: usize, says: u32, has: usize },
    /// The block repeats the block number of block `first`.
    Duplicate {
        block: usize,
        number: u32,
        first: usize,
    },
    /// No block carries block number `number`.
    Missing { number: u32 },
    /// The block writes addresses block `other` writes as well.
    Overlap { block: usize, other: usize },
}

impl core::fmt::Display for Uf2Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Empty => write!(f, "the file is empty"),
            Self::Length(len) => write!(
                f,
                "{len} bytes is not a whole number of {BLOCK}-byte blocks"
            ),
            Self::Magic { block, at, found } => {
                write!(f, "block {block}: magic word at byte {at} is {found:#010x}")
            }
            Self::NoFamily { block } => write!(f, "block {block}: no family-id flag"),
            Self::Family { block, found } => write!(
                f,
                "block {block}: family {found:#010x}, not rp2350-arm-s ({FAMILY_RP2350_ARM_S:#010x})"
            ),
            Self::NotMainFlash { block } => write!(
                f,
                "block {block}: marked not main flash, which the boot ROM ignores"
            ),
            Self::PayloadSize { block, size } => write!(
                f,
                "block {block}: a payload of {size} bytes; the boot ROM takes only {PAGE}"
            ),
            Self::Unaligned { block, addr } => write!(
                f,
                "block {block}: address {addr:#010x} is not a multiple of {PAGE}, which the boot ROM needs"
            ),
            Self::Address { block, addr, size } => write!(
                f,
                "block {block}: {size} bytes at {addr:#010x} lie outside the flash \
                 ({FLASH_BASE:#010x}, {FLASH_SIZE} bytes)"
            ),
            Self::NumBlocks { block, says, has } => {
                write!(f, "block {block}: says {says} blocks; the file has {has}")
            }
            Self::Duplicate {
                block,
                number,
                first,
            } => write!(
                f,
                "block {block}: block number {number} again (block {first})"
            ),
            Self::Missing { number } => write!(f, "no block has block number {number}"),
            Self::Overlap { block, other } => {
                write!(f, "block {block}: writes addresses block {other} writes")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Uf2Error {}

#[cfg(feature = "std")]
pub use host::{Image, Summary, check, from_elf};

#[cfg(feature = "std")]
mod host {
    use std::collections::BTreeMap;

    use super::{
        BLOCK, FAMILY_RP2350_ARM_S, FLAG_FAMILY, FLAG_NOT_MAIN_FLASH, FLASH_BASE, FLASH_SIZE,
        MAGIC, MAGIC_AT, PAGE, Uf2Error, block, image_def_offset, word,
    };

    /// What a UF2 that passes [`check`] writes.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Summary {
        pub blocks: usize,
        /// The lowest address a block writes.
        pub start: u32,
        /// The end of the highest block's payload.
        pub end: u32,
    }

    /// Checks every block of a UF2 against the rules the crate's writer
    /// follows and the boot ROM's drive needs: the three magic words, the
    /// family-id flag with family `rp2350-arm-s`, no not-main-flash flag, a
    /// payload of [`PAGE`] bytes at an address inside the flash and aligned
    /// to [`PAGE`], a block count every block states and the file has,
    /// block numbers 0..n each once, and no two blocks writing the same
    /// address.
    ///
    /// The boot ROM's block rules are the RP2350 datasheet's, section 5.5.2
    /// "UF2 format details": "All data destined for the device must be in
    /// UF2 blocks with: [...] A payload_size of 256"; for a regular flash
    /// image, "All blocks must be targeted at 256 byte alignments"; and "All
    /// device destined data must be in blocks without the
    /// UF2_FLAG_NOT_MAIN_FLASH marking". The drive takes a file whose
    /// blocks break them, but the boot ROM does not write those blocks, so
    /// the check refuses the file before any copy.
    ///
    /// # Errors
    ///
    /// The first rule broken, naming the block.
    pub fn check(image: &[u8]) -> Result<Summary, Uf2Error> {
        if image.is_empty() {
            return Err(Uf2Error::Empty);
        }
        if !image.len().is_multiple_of(BLOCK) {
            return Err(Uf2Error::Length(image.len()));
        }
        let has = image.len() / BLOCK;
        let mut seen: Vec<Option<usize>> = vec![None; has];
        let mut spans = Vec::with_capacity(has);
        for (i, b) in image.as_chunks::<BLOCK>().0.iter().enumerate() {
            let w = |at: usize| word(b, at).unwrap_or_default();
            for (at, magic) in MAGIC_AT.into_iter().zip(MAGIC) {
                if w(at) != magic {
                    return Err(Uf2Error::Magic {
                        block: i,
                        at,
                        found: w(at),
                    });
                }
            }
            if w(8) & FLAG_FAMILY == 0 {
                return Err(Uf2Error::NoFamily { block: i });
            }
            if w(28) != FAMILY_RP2350_ARM_S {
                return Err(Uf2Error::Family {
                    block: i,
                    found: w(28),
                });
            }
            if w(8) & FLAG_NOT_MAIN_FLASH != 0 {
                return Err(Uf2Error::NotMainFlash { block: i });
            }
            let (addr, size) = (w(12), w(16));
            if usize::try_from(size).ok() != Some(PAGE) {
                return Err(Uf2Error::PayloadSize { block: i, size });
            }
            let end = addr
                .checked_add(size)
                .filter(|&e| addr >= FLASH_BASE && e <= FLASH_BASE + FLASH_SIZE)
                .ok_or(Uf2Error::Address {
                    block: i,
                    addr,
                    size,
                })?;
            if usize::try_from(addr).is_ok_and(|a| !a.is_multiple_of(PAGE)) {
                return Err(Uf2Error::Unaligned { block: i, addr });
            }
            let says = w(24);
            if usize::try_from(says).ok() != Some(has) {
                return Err(Uf2Error::NumBlocks {
                    block: i,
                    says,
                    has,
                });
            }
            let number = w(20);
            // A number outside 0..n leaves one inside it missing.
            if let Some(slot) = usize::try_from(number).ok().and_then(|n| seen.get_mut(n)) {
                if let Some(first) = *slot {
                    return Err(Uf2Error::Duplicate {
                        block: i,
                        number,
                        first,
                    });
                }
                *slot = Some(i);
            }
            spans.push((addr, end, i));
        }
        if let Some(number) = seen.iter().position(Option::is_none) {
            return Err(Uf2Error::Missing {
                number: u32::try_from(number).unwrap_or(u32::MAX),
            });
        }
        spans.sort_unstable();
        for pair in spans.windows(2) {
            if let [(_, end, other), (addr, _, block)] = *pair
                && end > addr
            {
                return Err(Uf2Error::Overlap { block, other });
            }
        }
        Ok(Summary {
            blocks: has,
            start: spans.first().map_or(0, |s| s.0),
            end: spans.iter().map(|s| s.1).max().unwrap_or(0),
        })
    }

    /// A flash image: 256-byte pages by address, only pages some loadable
    /// segment touches, as the boot ROM's drive takes them.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Image {
        pages: BTreeMap<u32, [u8; PAGE]>,
    }

    fn u16_at(b: &[u8], at: usize) -> Result<u16, String> {
        b.get(at..at + 2)
            .map(|s| u16::from_le_bytes([s[0], s[1]]))
            .ok_or_else(|| format!("ELF truncated at {at}"))
    }

    fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
        b.get(at..at + 4)
            .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
            .ok_or_else(|| format!("ELF truncated at {at}"))
    }

    fn usize_of(v: u32) -> usize {
        usize::try_from(v).unwrap_or(usize::MAX)
    }

    /// The flash image of a 32-bit little-endian ELF: the file bytes of
    /// each loadable segment at its physical (load) address.
    ///
    /// # Errors
    ///
    /// Not such an ELF, or a loadable segment with file bytes outside the
    /// flash.
    pub fn from_elf(elf: &[u8]) -> Result<Image, String> {
        if elf.get(..6) != Some(&[0x7F, b'E', b'L', b'F', 1, 1][..]) {
            return Err("not a 32-bit little-endian ELF".into());
        }
        let phoff = usize_of(u32_at(elf, 0x1C)?);
        let phentsize = usize::from(u16_at(elf, 0x2A)?);
        let phnum = usize::from(u16_at(elf, 0x2C)?);
        let mut image = Image::default();
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            if u32_at(elf, ph)? != 1 {
                continue;
            }
            let offset = usize_of(u32_at(elf, ph + 4)?);
            let paddr = u32_at(elf, ph + 12)?;
            let filesz = u32_at(elf, ph + 16)?;
            if filesz == 0 {
                continue;
            }
            let end = paddr
                .checked_add(filesz)
                .ok_or("segment wraps the address space")?;
            if paddr < FLASH_BASE || end > FLASH_BASE + FLASH_SIZE {
                return Err(format!(
                    "segment at {paddr:#010x}..{end:#010x} is outside the flash"
                ));
            }
            let bytes = elf
                .get(offset..offset + usize_of(filesz))
                .ok_or_else(|| format!("segment at {paddr:#010x} runs past the file"))?;
            for (addr, &b) in (paddr..).zip(bytes) {
                let page = addr & !0xFF;
                let at = usize_of(addr - page);
                if let Some(slot) = image.pages.entry(page).or_insert([0; PAGE]).get_mut(at) {
                    *slot = b;
                }
            }
        }
        if image.pages.is_empty() {
            return Err("no loadable bytes".into());
        }
        Ok(image)
    }

    impl Image {
        /// The image's bytes from its first page, gaps zero-filled.
        #[must_use]
        pub fn flat(&self) -> Vec<u8> {
            let Some((&first, _)) = self.pages.first_key_value() else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for (&addr, page) in &self.pages {
                out.resize(usize_of(addr - first), 0);
                out.extend_from_slice(page);
            }
            out
        }

        /// The UF2 file.
        ///
        /// # Errors
        ///
        /// No image definition in the first 4 KiB of the flash, or an image
        /// that does not start at the flash's base.
        pub fn uf2(&self) -> Result<Vec<u8>, String> {
            if self.pages.first_key_value().map(|(&a, _)| a) != Some(FLASH_BASE) {
                return Err(format!("the image does not start at {FLASH_BASE:#010x}"));
            }
            if image_def_offset(&self.flat()).is_none() {
                return Err("no image definition in the first 4 KiB".into());
            }
            let total = u32::try_from(self.pages.len()).map_err(|e| e.to_string())?;
            let mut out = Vec::with_capacity(self.pages.len() * 512);
            for ((&addr, page), index) in self.pages.iter().zip(0..) {
                out.extend_from_slice(&block(index, total, addr, page));
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An image definition as embassy-rp's default `secure_exe` writes it.
    const IMAGE_DEF: [u32; 5] = [BLOCK_START, 0x1021_0142, 0x0000_01FF, 0, BLOCK_END];

    fn image_with_def_at(at: usize, len: usize) -> Vec<u8> {
        let mut image = vec![0xAA; len];
        for (i, w) in IMAGE_DEF.iter().enumerate() {
            image[at + 4 * i..at + 4 * i + 4].copy_from_slice(&w.to_le_bytes());
        }
        image
    }

    #[test]
    fn the_image_definition_is_found_only_in_the_first_4_kib() {
        assert_eq!(
            image_def_offset(&image_with_def_at(0x114, 8192)),
            Some(0x114)
        );
        assert_eq!(image_def_offset(&image_with_def_at(4096, 8192)), None);
        assert_eq!(
            image_def_offset(&image_with_def_at(0x115, 8192)),
            None,
            "unaligned"
        );
        let mut no_type = image_with_def_at(0x114, 8192);
        no_type[0x118] = 0x41;
        assert_eq!(
            image_def_offset(&no_type),
            None,
            "first item not an image type"
        );
        assert_eq!(image_def_offset(&[0; 64]), None);
    }

    #[test]
    fn a_block_has_its_header_payload_and_end() {
        let b = block(2, 107, 0x1000_0200, &[1, 2, 3]);
        let w = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        assert_eq!(
            [w(0), w(4), w(8), w(12), w(16), w(20), w(24), w(28)],
            [
                MAGIC[0],
                MAGIC[1],
                FLAG_FAMILY,
                0x1000_0200,
                256,
                2,
                107,
                FAMILY_RP2350_ARM_S
            ]
        );
        assert_eq!(&b[32..36], &[1, 2, 3, 0]);
        assert_eq!(w(508), MAGIC[2]);
    }

    /// A minimal ELF: one loadable segment of `bytes` at `paddr`.
    fn elf(paddr: u32, bytes: &[u8]) -> Vec<u8> {
        let mut e = vec![0_u8; 0x34 + 0x20];
        e[..6].copy_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1]);
        e[0x1C..0x20].copy_from_slice(&0x34_u32.to_le_bytes());
        e[0x2A..0x2C].copy_from_slice(&0x20_u16.to_le_bytes());
        e[0x2C..0x2E].copy_from_slice(&1_u16.to_le_bytes());
        let ph = 0x34;
        let offset = u32::try_from(e.len()).unwrap();
        let len = u32::try_from(bytes.len()).unwrap();
        for (at, v) in [
            (0, 1),
            (4, offset),
            (8, paddr),
            (12, paddr),
            (16, len),
            (20, len),
        ] {
            e[ph + at..ph + at + 4].copy_from_slice(&v.to_le_bytes());
        }
        e.extend_from_slice(bytes);
        e
    }

    #[test]
    fn an_elf_becomes_pages_and_blocks() {
        let bytes = image_with_def_at(0x114, 300);
        let image = from_elf(&elf(FLASH_BASE, &bytes)).unwrap();
        let mut flat = bytes.clone();
        flat.resize(512, 0);
        assert_eq!(image.flat(), flat);
        let uf2 = image.uf2().unwrap();
        assert_eq!(uf2.len(), 2 * 512);
        assert_eq!(&uf2[512 + 12..512 + 16], &(FLASH_BASE + 256).to_le_bytes());
    }

    /// A three-block UF2 from the crate's writer.
    fn written() -> Vec<u8> {
        let bytes = image_with_def_at(0x114, 700);
        from_elf(&elf(FLASH_BASE, &bytes)).unwrap().uf2().unwrap()
    }

    /// `uf2` with the word at byte `at` of block `block` set to `v`.
    fn set(mut uf2: Vec<u8>, block: usize, at: usize, v: u32) -> Vec<u8> {
        let at = block * BLOCK + at;
        uf2[at..at + 4].copy_from_slice(&v.to_le_bytes());
        uf2
    }

    #[test]
    fn the_writers_uf2_passes_the_check() {
        assert_eq!(
            check(&written()),
            Ok(Summary {
                blocks: 3,
                start: FLASH_BASE,
                end: FLASH_BASE + 768,
            })
        );
    }

    #[test]
    fn a_uf2_of_no_whole_blocks_is_refused() {
        assert_eq!(check(&[]), Err(Uf2Error::Empty));
        let mut short = written();
        short.pop();
        assert_eq!(check(&short), Err(Uf2Error::Length(3 * BLOCK - 1)));
    }

    #[test]
    fn each_magic_word_is_checked() {
        for at in MAGIC_AT {
            assert_eq!(
                check(&set(written(), 1, at, 0)),
                Err(Uf2Error::Magic {
                    block: 1,
                    at,
                    found: 0
                }),
                "magic at {at}"
            );
        }
    }

    #[test]
    fn a_block_must_name_the_rp2350_arm_secure_family() {
        assert_eq!(
            check(&set(written(), 2, 8, 0)),
            Err(Uf2Error::NoFamily { block: 2 })
        );
        // The rp2350-arm-ns family.
        assert_eq!(
            check(&set(written(), 2, 28, 0xE48B_FF5B)),
            Err(Uf2Error::Family {
                block: 2,
                found: 0xE48B_FF5B
            })
        );
    }

    /// The boot ROM writes only blocks of a 256-byte payload.
    #[test]
    fn a_payload_must_be_256_bytes() {
        for size in [0, 1, 255, 257, 476] {
            assert_eq!(
                check(&set(written(), 2, 16, size)),
                Err(Uf2Error::PayloadSize { block: 2, size }),
                "{size}"
            );
        }
    }

    /// The boot ROM writes a flash block only at a 256-byte boundary.
    #[test]
    fn a_block_must_target_a_256_byte_boundary() {
        for addr in [FLASH_BASE + 2 * 256 + 1, FLASH_BASE + 2 * 256 + 128] {
            assert_eq!(
                check(&set(written(), 2, 12, addr)),
                Err(Uf2Error::Unaligned { block: 2, addr }),
                "{addr:#x}"
            );
        }
    }

    /// The boot ROM ignores a block marked not main flash.
    #[test]
    fn a_block_marked_not_main_flash_is_refused() {
        assert_eq!(
            check(&set(written(), 1, 8, FLAG_FAMILY | FLAG_NOT_MAIN_FLASH)),
            Err(Uf2Error::NotMainFlash { block: 1 })
        );
    }

    #[test]
    fn a_block_must_write_inside_the_flash() {
        let last = FLASH_BASE + FLASH_SIZE - 256;
        assert!(check(&set(written(), 2, 12, last)).is_ok());
        for addr in [FLASH_BASE - 256, last + 1, 0x2000_0000, u32::MAX - 16] {
            assert_eq!(
                check(&set(written(), 2, 12, addr)),
                Err(Uf2Error::Address {
                    block: 2,
                    addr,
                    size: 256
                }),
                "{addr:#x}"
            );
        }
    }

    #[test]
    fn every_block_states_the_files_block_count() {
        assert_eq!(
            check(&set(written(), 1, 24, 4)),
            Err(Uf2Error::NumBlocks {
                block: 1,
                says: 4,
                has: 3
            })
        );
    }

    #[test]
    fn block_numbers_run_from_0_each_once() {
        assert_eq!(
            check(&set(written(), 2, 20, 0)),
            Err(Uf2Error::Duplicate {
                block: 2,
                number: 0,
                first: 0
            })
        );
        assert_eq!(
            check(&set(written(), 1, 20, 3)),
            Err(Uf2Error::Missing { number: 1 })
        );
    }

    #[test]
    fn no_two_blocks_write_the_same_address() {
        // With 256-byte payloads at 256-byte boundaries, two blocks overlap
        // only at the same address.
        assert_eq!(
            check(&set(written(), 2, 12, FLASH_BASE)),
            Err(Uf2Error::Overlap { block: 2, other: 0 })
        );
        // Adjacent blocks touch without overlapping.
        assert!(check(&set(written(), 2, 12, FLASH_BASE + 0x1000)).is_ok());
    }

    #[test]
    fn an_image_without_its_definition_is_refused() {
        let image = from_elf(&elf(FLASH_BASE, &[0xAA; 600])).unwrap();
        assert_eq!(
            image.uf2().unwrap_err(),
            "no image definition in the first 4 KiB"
        );
        assert!(
            from_elf(&elf(0x2000_0000, &[1]))
                .unwrap_err()
                .contains("outside the flash")
        );
        assert!(from_elf(b"\x7fELF\x02\x01").is_err());
    }
}
