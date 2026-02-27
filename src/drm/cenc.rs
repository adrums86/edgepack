use crate::error::{EdgePackagerError, Result};
use aes::Aes128;
use cipher::{KeyIvInit, StreamCipher};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// CENC (Common Encryption - CTR mode) encryptor.
///
/// CENC uses AES-128-CTR for full sample encryption (no pattern).
/// Each sample gets a unique IV, and the counter increments across
/// the 16-byte blocks of the sample.
///
/// Reference: ISO/IEC 23001-7 (CENC) Section 10.1
pub struct CencEncryptor {
    key: [u8; 16],
}

impl CencEncryptor {
    pub fn new(key: [u8; 16]) -> Self {
        Self { key }
    }

    /// Encrypt a single sample in place using AES-128-CTR.
    ///
    /// * `data` — the plaintext sample data (modified in place)
    /// * `iv` — 8 or 16 byte initialization vector for this sample.
    ///   If 8 bytes, the IV forms the upper 8 bytes of the 16-byte counter block,
    ///   with the lower 8 bytes starting at 0 (block counter).
    /// * `subsamples` — optional subsample map (clear_bytes, encrypted_bytes).
    ///   If provided, only the encrypted portions of each subsample are encrypted.
    ///   If None, the entire sample is encrypted.
    pub fn encrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()> {
        let counter = build_counter_block(iv)?;

        match subsamples {
            Some(subs) => self.encrypt_with_subsamples(data, &counter, subs),
            None => self.encrypt_full(data, &counter),
        }
    }

    /// Encrypt the entire data buffer with AES-128-CTR.
    fn encrypt_full(&self, data: &mut [u8], counter: &[u8; 16]) -> Result<()> {
        let mut cipher = Aes128Ctr::new(&self.key.into(), counter.into());
        cipher.apply_keystream(data);
        Ok(())
    }

    /// Encrypt only the encrypted portions of subsamples.
    fn encrypt_with_subsamples(
        &self,
        data: &mut [u8],
        counter: &[u8; 16],
        subsamples: &[(u32, u32)],
    ) -> Result<()> {
        let mut cipher = Aes128Ctr::new(&self.key.into(), counter.into());
        let mut offset = 0usize;

        for &(clear_bytes, encrypted_bytes) in subsamples {
            // Skip clear bytes (but DON'T advance the cipher — CENC only
            // counts encrypted bytes toward the counter)
            offset += clear_bytes as usize;

            if encrypted_bytes > 0 {
                let end = offset + encrypted_bytes as usize;
                if end > data.len() {
                    return Err(EdgePackagerError::Encryption(
                        "subsample extends beyond sample data".into(),
                    ));
                }
                cipher.apply_keystream(&mut data[offset..end]);
                offset = end;
            }
        }

        Ok(())
    }
}

/// Generate a per-sample IV for CENC encryption.
///
/// Returns an 8-byte IV derived from the segment number and sample index.
/// The IV is designed to be unique across all samples in the content.
pub fn generate_sample_iv(segment_number: u32, sample_index: u32) -> [u8; 8] {
    let mut iv = [0u8; 8];
    iv[0..4].copy_from_slice(&segment_number.to_be_bytes());
    iv[4..8].copy_from_slice(&sample_index.to_be_bytes());
    iv
}

/// Build a 16-byte counter block from an 8-byte or 16-byte IV.
///
/// For 8-byte IVs: upper 8 bytes = IV, lower 8 bytes = 0 (block counter).
/// For 16-byte IVs: used directly as the counter block.
fn build_counter_block(iv: &[u8]) -> Result<[u8; 16]> {
    match iv.len() {
        8 => {
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(iv);
            Ok(block)
        }
        16 => {
            let mut block = [0u8; 16];
            block.copy_from_slice(iv);
            Ok(block)
        }
        other => Err(EdgePackagerError::Encryption(format!(
            "CENC IV must be 8 or 16 bytes, got {other}"
        ))),
    }
}
