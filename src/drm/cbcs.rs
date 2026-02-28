use crate::error::{EdgepackError, Result};
use aes::Aes128;
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit};

type Aes128CbcDec = cbc::Decryptor<Aes128>;
type Aes128CbcEnc = cbc::Encryptor<Aes128>;

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
            return Err(EdgepackError::Encryption(format!(
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
                    return Err(EdgepackError::Encryption(
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
                        EdgepackError::Encryption(format!("CBCS decrypt error: {e}"))
                    })?;

                block_idx += blocks_to_decrypt;
            }
        }

        Ok(())
    }
}

/// CBCS (Common encryption scheme - CBC mode with pattern encryption) encryptor.
///
/// Mirrors `CbcsDecryptor` but encrypts data instead of decrypting.
/// Used when the target encryption scheme is CBCS.
///
/// Reference: ISO/IEC 23001-7 (CENC) Section 10.4
pub struct CbcsEncryptor {
    key: [u8; 16],
    /// Number of 16-byte blocks to encrypt in each pattern cycle.
    crypt_byte_block: u8,
    /// Number of 16-byte blocks to skip in each pattern cycle.
    skip_byte_block: u8,
}

impl CbcsEncryptor {
    /// Create a new CBCS encryptor.
    ///
    /// * `key` — 16-byte AES key
    /// * `crypt_byte_block` — blocks to encrypt per pattern (typically 1 for video)
    /// * `skip_byte_block` — blocks to skip per pattern (typically 9 for video)
    ///
    /// If both crypt and skip are 0, the entire sample is encrypted (used for audio).
    pub fn new(key: [u8; 16], crypt_byte_block: u8, skip_byte_block: u8) -> Self {
        Self {
            key,
            crypt_byte_block,
            skip_byte_block,
        }
    }

    /// Encrypt a single sample (NAL unit or audio frame) in place.
    ///
    /// * `data` — the plaintext sample data (modified in place)
    /// * `iv` — 16-byte initialization vector for this sample
    /// * `subsamples` — optional subsample encryption map (clear_bytes, encrypted_bytes) pairs.
    ///   If None, the entire sample is subject to pattern encryption.
    pub fn encrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()> {
        if iv.len() != 16 {
            return Err(EdgepackError::Encryption(format!(
                "CBCS IV must be 16 bytes, got {}",
                iv.len()
            )));
        }

        let iv_array: [u8; 16] = iv.try_into().unwrap();

        match subsamples {
            Some(subs) => self.encrypt_with_subsamples(data, &iv_array, subs),
            None => self.encrypt_pattern(data, &iv_array),
        }
    }

    /// Encrypt with subsample mapping.
    fn encrypt_with_subsamples(
        &self,
        data: &mut [u8],
        iv: &[u8; 16],
        subsamples: &[(u32, u32)],
    ) -> Result<()> {
        let mut offset = 0usize;

        for &(clear_bytes, encrypted_bytes) in subsamples {
            offset += clear_bytes as usize;

            if encrypted_bytes > 0 {
                let end = offset + encrypted_bytes as usize;
                if end > data.len() {
                    return Err(EdgepackError::Encryption(
                        "subsample extends beyond sample data".into(),
                    ));
                }
                let encrypted_region = &mut data[offset..end];
                self.encrypt_pattern(encrypted_region, iv)?;
                offset = end;
            }
        }

        Ok(())
    }

    /// Apply CBCS pattern encryption to a data region.
    ///
    /// Pattern: encrypt `crypt_byte_block` blocks, skip `skip_byte_block` blocks, repeat.
    /// If both are 0, encrypt all complete blocks.
    /// Any trailing bytes less than a full block are left in the clear.
    fn encrypt_pattern(&self, data: &mut [u8], iv: &[u8; 16]) -> Result<()> {
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

            let blocks_to_encrypt = if full_pattern {
                total_blocks - block_idx
            } else if pos_in_pattern < crypt_count {
                (crypt_count - pos_in_pattern).min(total_blocks - block_idx)
            } else {
                let skip_remaining = pattern_len - pos_in_pattern;
                block_idx += skip_remaining;
                continue;
            };

            if blocks_to_encrypt > 0 {
                let start = block_idx * block_size;
                let end = start + blocks_to_encrypt * block_size;

                // CBCS: each encrypted range uses the same IV (reset per sample, not chained)
                let encryptor = Aes128CbcEnc::new(&self.key.into(), &(*iv).into());
                encryptor
                    .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(
                        &mut data[start..end],
                        end - start,
                    )
                    .map_err(|e| {
                        EdgepackError::Encryption(format!("CBCS encrypt error: {e}"))
                    })?;

                block_idx += blocks_to_encrypt;
            }
        }

        Ok(())
    }

    /// Generate a per-sample IV for CBCS encryption.
    ///
    /// Returns a 16-byte IV derived from the segment number and sample index.
    /// CBCS requires 16-byte IVs for CBC mode.
    pub fn generate_iv(segment_number: u32, sample_index: u32) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[0..4].copy_from_slice(&segment_number.to_be_bytes());
        iv[4..8].copy_from_slice(&sample_index.to_be_bytes());
        iv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: encrypt data with AES-128-CBC (no padding) to create test ciphertext.
    fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
        let encryptor = Aes128CbcEnc::new(key.into(), iv.into());
        encryptor
            .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(data, data.len())
            .unwrap();
    }

    #[test]
    fn decrypt_sample_rejects_short_iv() {
        let dec = CbcsDecryptor::new([0u8; 16], 0, 0);
        let mut data = [0u8; 16];
        let result = dec.decrypt_sample(&mut data, &[0u8; 8], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("16 bytes"));
    }

    #[test]
    fn decrypt_sample_data_smaller_than_block_is_noop() {
        let dec = CbcsDecryptor::new([0u8; 16], 0, 0);
        let original = [0xAA; 15];
        let mut data = original;
        dec.decrypt_sample(&mut data, &[0u8; 16], None).unwrap();
        assert_eq!(data, original); // unchanged
    }

    #[test]
    fn decrypt_full_pattern_single_block() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];
        let plaintext = [0xDE; 16];
        let mut data = plaintext;
        cbc_encrypt(&key, &iv, &mut data);
        assert_ne!(data, plaintext); // verify encryption changed data

        let dec = CbcsDecryptor::new(key, 0, 0); // 0:0 = full encryption
        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn decrypt_full_pattern_multiple_blocks() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];
        let plaintext = [0xDE; 48]; // 3 blocks
        let mut data = plaintext;
        cbc_encrypt(&key, &iv, &mut data);

        let dec = CbcsDecryptor::new(key, 0, 0);
        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn decrypt_pattern_1_9_only_first_block_encrypted() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];

        // Create 10 blocks (160 bytes): pattern 1:9 means block 0 encrypted, blocks 1-9 clear
        let mut plaintext = vec![0u8; 160];
        for (i, b) in plaintext.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        let mut data = plaintext.clone();

        // Encrypt only the first block (pattern 1:9)
        cbc_encrypt(&key, &iv, &mut data[0..16]);

        let dec = CbcsDecryptor::new(key, 1, 9);
        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn decrypt_with_subsamples() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];

        // 5 clear bytes + 32 encrypted bytes (2 blocks) = 37 bytes total
        let mut plaintext = vec![0u8; 37];
        for (i, b) in plaintext.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }
        let mut data = plaintext.clone();

        // Encrypt the encrypted portion (bytes 5..37, which is 32 bytes = 2 blocks)
        cbc_encrypt(&key, &iv, &mut data[5..37]);

        let dec = CbcsDecryptor::new(key, 0, 0); // full encryption in encrypted region
        let subsamples = [(5u32, 32u32)];
        dec.decrypt_sample(&mut data, &iv, Some(&subsamples)).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn decrypt_subsample_out_of_bounds_errors() {
        let dec = CbcsDecryptor::new([0u8; 16], 0, 0);
        let mut data = [0u8; 16];
        let subsamples = [(0u32, 32u32)]; // 32 bytes > 16 bytes of data
        let result = dec.decrypt_sample(&mut data, &[0u8; 16], Some(&subsamples));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("beyond sample data"));
    }

    #[test]
    fn decrypt_trailing_bytes_left_in_clear() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];
        // 20 bytes = 1 full block + 4 trailing bytes
        let plaintext = vec![0xAA; 20];
        let mut data = plaintext.clone();
        // Only encrypt the first block
        cbc_encrypt(&key, &iv, &mut data[0..16]);
        // Trailing 4 bytes should be unchanged
        assert_eq!(&data[16..20], &plaintext[16..20]);

        let dec = CbcsDecryptor::new(key, 0, 0);
        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    // === CbcsEncryptor tests ===

    #[test]
    fn encrypt_sample_rejects_short_iv() {
        let enc = CbcsEncryptor::new([0u8; 16], 0, 0);
        let mut data = [0u8; 16];
        let result = enc.encrypt_sample(&mut data, &[0u8; 8], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("16 bytes"));
    }

    #[test]
    fn encrypt_decrypt_roundtrip_full_pattern() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];
        let plaintext = vec![0xDE; 48]; // 3 blocks

        let mut data = plaintext.clone();
        let enc = CbcsEncryptor::new(key, 0, 0);
        enc.encrypt_sample(&mut data, &iv, None).unwrap();
        assert_ne!(data, plaintext);

        let dec = CbcsDecryptor::new(key, 0, 0);
        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn encrypt_decrypt_roundtrip_pattern_1_9() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];
        let mut plaintext = vec![0u8; 160]; // 10 blocks
        for (i, b) in plaintext.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }

        let mut data = plaintext.clone();
        let enc = CbcsEncryptor::new(key, 1, 9);
        enc.encrypt_sample(&mut data, &iv, None).unwrap();
        // Only first block should be changed
        assert_ne!(&data[0..16], &plaintext[0..16]);
        assert_eq!(&data[16..], &plaintext[16..]); // blocks 1-9 unchanged

        let dec = CbcsDecryptor::new(key, 1, 9);
        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn encrypt_decrypt_roundtrip_with_subsamples() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];
        let mut plaintext = vec![0u8; 37]; // 5 clear + 32 encrypted
        for (i, b) in plaintext.iter_mut().enumerate() {
            *b = (i & 0xFF) as u8;
        }

        let mut data = plaintext.clone();
        let subsamples = [(5u32, 32u32)];
        let enc = CbcsEncryptor::new(key, 0, 0);
        enc.encrypt_sample(&mut data, &iv, Some(&subsamples)).unwrap();
        // Clear portion unchanged
        assert_eq!(&data[0..5], &plaintext[0..5]);
        // Encrypted portion changed
        assert_ne!(&data[5..37], &plaintext[5..37]);

        let dec = CbcsDecryptor::new(key, 0, 0);
        dec.decrypt_sample(&mut data, &iv, Some(&subsamples)).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn encrypt_data_smaller_than_block_is_noop() {
        let enc = CbcsEncryptor::new([0u8; 16], 0, 0);
        let original = [0xAA; 15];
        let mut data = original;
        enc.encrypt_sample(&mut data, &[0u8; 16], None).unwrap();
        assert_eq!(data, original);
    }

    #[test]
    fn encrypt_subsample_out_of_bounds_errors() {
        let enc = CbcsEncryptor::new([0u8; 16], 0, 0);
        let mut data = [0u8; 16];
        let subsamples = [(0u32, 32u32)];
        let result = enc.encrypt_sample(&mut data, &[0u8; 16], Some(&subsamples));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("beyond sample data"));
    }

    #[test]
    fn generate_iv_deterministic() {
        let iv1 = CbcsEncryptor::generate_iv(0, 0);
        let iv2 = CbcsEncryptor::generate_iv(0, 0);
        assert_eq!(iv1, iv2);
    }

    #[test]
    fn generate_iv_unique_per_sample() {
        let iv1 = CbcsEncryptor::generate_iv(0, 0);
        let iv2 = CbcsEncryptor::generate_iv(0, 1);
        assert_ne!(iv1, iv2);
    }

    #[test]
    fn generate_iv_is_16_bytes() {
        let iv = CbcsEncryptor::generate_iv(42, 7);
        assert_eq!(iv.len(), 16);
    }
}
