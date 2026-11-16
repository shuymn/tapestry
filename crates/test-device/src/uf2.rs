//! The UF2 writer for the RP2350's boot ROM drive (docs/test-device.md
//! "Building"): 512-byte blocks of 256 payload bytes, family
//! `rp2350-arm-s`, written only for an image whose image definition lies
//! where the boot ROM looks for it.

/// UF2's first and second magic words and its final magic word.
const MAGIC: [u32; 3] = [0x0A32_4655, 0x9E5D_5157, 0x0AB1_6F30];
/// The block carries a family id.
const FLAG_FAMILY: u32 = 0x0000_2000;
/// The `rp2350-arm-s` family id.
pub const FAMILY_RP2350_ARM_S: u32 = 0xE48B_FF59;
/// The flash's address.
pub const FLASH_BASE: u32 = 0x1000_0000;
/// The Pico 2's flash size.
pub const FLASH_SIZE: u32 = 4 << 20;
/// Payload bytes per block.
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
pub fn block(index: u32, total: u32, addr: u32, payload: &[u8]) -> [u8; 512] {
    let mut out = [0_u8; 512];
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

#[cfg(feature = "std")]
pub use host::{Image, from_elf};

#[cfg(feature = "std")]
mod host {
    use std::collections::BTreeMap;

    use super::{FLASH_BASE, FLASH_SIZE, PAGE, block, image_def_offset};

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
