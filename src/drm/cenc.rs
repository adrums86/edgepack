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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_sample_iv_deterministic() {
        let iv1 = generate_sample_iv(0, 0);
        let iv2 = generate_sample_iv(0, 0);
        assert_eq!(iv1, iv2);
    }

    #[test]
    fn generate_sample_iv_unique_per_sample() {
        let iv1 = generate_sample_iv(0, 0);
        let iv2 = generate_sample_iv(0, 1);
        assert_ne!(iv1, iv2);
    }

    #[test]
    fn generate_sample_iv_unique_per_segment() {
        let iv1 = generate_sample_iv(0, 0);
        let iv2 = generate_sample_iv(1, 0);
        assert_ne!(iv1, iv2);
    }

    #[test]
    fn generate_sample_iv_length() {
        let iv = generate_sample_iv(42, 7);
        assert_eq!(iv.len(), 8);
    }

    #[test]
    fn generate_sample_iv_encodes_segment_and_sample() {
        let iv = generate_sample_iv(1, 2);
        assert_eq!(&iv[0..4], &1u32.to_be_bytes());
        assert_eq!(&iv[4..8], &2u32.to_be_bytes());
    }

    #[test]
    fn build_counter_block_8_byte_iv() {
        let iv = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let block = build_counter_block(&iv).unwrap();
        assert_eq!(&block[..8], &iv);
        assert_eq!(&block[8..], &[0u8; 8]); // lower 8 bytes are zero
    }

    #[test]
    fn build_counter_block_16_byte_iv() {
        let iv = [0xAA; 16];
        let block = build_counter_block(&iv).unwrap();
        assert_eq!(block, iv);
    }

    #[test]
    fn build_counter_block_invalid_length() {
        assert!(build_counter_block(&[0u8; 4]).is_err());
        assert!(build_counter_block(&[0u8; 12]).is_err());
        assert!(build_counter_block(&[]).is_err());
    }

    #[test]
    fn encrypt_decrypt_roundtrip_full_sample() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 8];
        let plaintext = vec![0xDE; 64]; // 4 blocks

        let mut encrypted = plaintext.clone();
        let enc = CencEncryptor::new(key);
        enc.encrypt_sample(&mut encrypted, &iv, None).unwrap();
        assert_ne!(encrypted, plaintext); // encryption changed data

        // CTR mode is symmetric — encrypting again decrypts
        let mut decrypted = encrypted.clone();
        enc.encrypt_sample(&mut decrypted, &iv, None).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_roundtrip_with_subsamples() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 8];
        // 10 clear + 32 encrypted + 5 clear = 47 bytes
        let plaintext = vec![0xBB; 47];
        let subsamples = [(10u32, 32u32), (5u32, 0u32)];

        let mut encrypted = plaintext.clone();
        let enc = CencEncryptor::new(key);
        enc.encrypt_sample(&mut encrypted, &iv, Some(&subsamples)).unwrap();

        // Clear portions unchanged
        assert_eq!(&encrypted[0..10], &plaintext[0..10]);
        assert_eq!(&encrypted[42..47], &plaintext[42..47]);
        // Encrypted portion changed
        assert_ne!(&encrypted[10..42], &plaintext[10..42]);

        // Decrypt (CTR is symmetric)
        let mut decrypted = encrypted;
        enc.encrypt_sample(&mut decrypted, &iv, Some(&subsamples)).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_subsample_out_of_bounds() {
        let enc = CencEncryptor::new([0u8; 16]);
        let mut data = [0u8; 16];
        let subsamples = [(0u32, 32u32)];
        let result = enc.encrypt_sample(&mut data, &[0u8; 8], Some(&subsamples));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("beyond sample data"));
    }

    #[test]
    fn encrypt_rejects_invalid_iv_length() {
        let enc = CencEncryptor::new([0u8; 16]);
        let mut data = [0u8; 16];
        let result = enc.encrypt_sample(&mut data, &[0u8; 5], None);
        assert!(result.is_err());
    }

    #[test]
    fn encrypt_empty_data() {
        let enc = CencEncryptor::new([0u8; 16]);
        let mut data = vec![];
        // Encrypting empty data should be fine
        enc.encrypt_sample(&mut data, &[0u8; 8], None).unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn different_keys_produce_different_ciphertext() {
        let iv = [0u8; 8];
        let plaintext = vec![0xAA; 32];

        let mut enc1 = plaintext.clone();
        CencEncryptor::new([0x01; 16]).encrypt_sample(&mut enc1, &iv, None).unwrap();

        let mut enc2 = plaintext.clone();
        CencEncryptor::new([0x02; 16]).encrypt_sample(&mut enc2, &iv, None).unwrap();

        assert_ne!(enc1, enc2);
    }

    #[test]
    fn different_ivs_produce_different_ciphertext() {
        let key = [0x42; 16];
        let plaintext = vec![0xAA; 32];

        let mut enc1 = plaintext.clone();
        CencEncryptor::new(key).encrypt_sample(&mut enc1, &[0x00; 8], None).unwrap();

        let mut enc2 = plaintext.clone();
        CencEncryptor::new(key).encrypt_sample(&mut enc2, &[0x01; 8], None).unwrap();

        assert_ne!(enc1, enc2);
    }
}
