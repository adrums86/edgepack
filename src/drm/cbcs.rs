use crate::error::{EdgePackagerError, Result};
use aes::Aes128;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};

type Aes128CbcDec = cbc::Decryptor<Aes128>;

/// CBCS (Common encryption scheme - CBC mode with pattern encryption) decryptor.
///
/// CBCS uses AES-128-CBC with a pattern of encrypted and clear 16-byte blocks.
/// The default pattern for video is 1:9 (encrypt 1 block, skip 9 blocks).
/// Audio is typically fully encrypted (pattern 0:0 means encrypt all).
///
/// Reference: ISO/IEC 23001-7 (CENC) Section 10.4
pub struct CbcsDecryptor {
    key: [u8; 16],
    /// Number of 16-byte blocks to encrypt in each pattern cycle.
    crypt_byte_block: u8,
    /// Number of 16-byte blocks to skip in each pattern cycle.
    skip_byte_block: u8,
}

impl CbcsDecryptor {
    /// Create a new CBCS decryptor.
    ///
    /// * `key` — 16-byte AES key
    /// * `crypt_byte_block` — blocks to encrypt per pattern (from tenc box, typically 1)
    /// * `skip_byte_block` — blocks to skip per pattern (from tenc box, typically 9)
    ///
    /// If both crypt and skip are 0, the entire sample is encrypted (used for audio).
    pub fn new(key: [u8; 16], crypt_byte_block: u8, skip_byte_block: u8) -> Self {
        Self {
            key,
            crypt_byte_block,
            skip_byte_block,
        }
    }

    /// Decrypt a single sample (NAL unit or audio frame) in place.
    ///
    /// * `data` — the encrypted sample data (modified in place)
    /// * `iv` — 16-byte initialization vector for this sample
    /// * `subsamples` — optional subsample encryption map (clear_bytes, encrypted_bytes) pairs.
    ///   If None, the entire sample is subject to pattern encryption.
    pub fn decrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()> {
        if iv.len() != 16 {
            return Err(EdgePackagerError::Encryption(format!(
                "CBCS IV must be 16 bytes, got {}",
                iv.len()
            )));
        }

        let iv_array: [u8; 16] = iv.try_into().unwrap();

        match subsamples {
            Some(subs) => self.decrypt_with_subsamples(data, &iv_array, subs),
            None => self.decrypt_pattern(data, &iv_array),
        }
    }

    /// Decrypt with subsample mapping.
    /// Each subsample defines (clear_bytes, encrypted_bytes).
    /// Only the encrypted portions are subject to pattern decryption.
    fn decrypt_with_subsamples(
        &self,
        data: &mut [u8],
        iv: &[u8; 16],
        subsamples: &[(u32, u32)],
    ) -> Result<()> {
        let mut offset = 0usize;

        for &(clear_bytes, encrypted_bytes) in subsamples {
            // Skip clear bytes
            offset += clear_bytes as usize;

            if encrypted_bytes > 0 {
                let end = offset + encrypted_bytes as usize;
                if end > data.len() {
                    return Err(EdgePackagerError::Encryption(
                        "subsample extends beyond sample data".into(),
                    ));
                }
                let encrypted_region = &mut data[offset..end];
                self.decrypt_pattern(encrypted_region, iv)?;
                offset = end;
            }
        }

        Ok(())
    }

    /// Apply CBCS pattern decryption to a data region.
    ///
    /// Pattern: encrypt `crypt_byte_block` blocks, skip `skip_byte_block` blocks, repeat.
    /// If both are 0, encrypt all complete blocks.
    /// Any trailing bytes less than a full block are left in the clear.
    fn decrypt_pattern(&self, data: &mut [u8], iv: &[u8; 16]) -> Result<()> {
        let block_size = 16usize;
        let total_blocks = data.len() / block_size;

        if total_blocks == 0 {
            return Ok(()); // Data smaller than one block — left in clear
        }

        let full_pattern = self.crypt_byte_block == 0 && self.skip_byte_block == 0;
        let pattern_len = if full_pattern {
            total_blocks
        } else {
            (self.crypt_byte_block + self.skip_byte_block) as usize
        };

        let crypt_count = if full_pattern {
            total_blocks
        } else {
            self.crypt_byte_block as usize
        };

        let mut block_idx = 0;

        while block_idx < total_blocks {
            let pos_in_pattern = if full_pattern {
                0
            } else {
                block_idx % pattern_len
            };

            let blocks_to_decrypt = if full_pattern {
                total_blocks - block_idx
            } else if pos_in_pattern < crypt_count {
                // We're in the "crypt" portion of the pattern
                (crypt_count - pos_in_pattern).min(total_blocks - block_idx)
            } else {
                // We're in the "skip" portion — advance to next pattern cycle
                let skip_remaining = pattern_len - pos_in_pattern;
                block_idx += skip_remaining;
                continue;
            };

            if blocks_to_decrypt > 0 {
                let start = block_idx * block_size;
                let end = start + blocks_to_decrypt * block_size;

                // CBCS: each encrypted range uses the same IV (reset per sample, not chained)
                let decryptor = Aes128CbcDec::new(&self.key.into(), &(*iv).into());
                decryptor
                    .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(
                        &mut data[start..end],
                    )
                    .map_err(|e| {
                        EdgePackagerError::Encryption(format!("CBCS decrypt error: {e}"))
                    })?;

                block_idx += blocks_to_decrypt;
            }
        }

        Ok(())
    }
}
