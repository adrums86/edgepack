use crate::drm::cbcs::{CbcsDecryptor, CbcsEncryptor};
use crate::drm::cenc::{self, CencDecryptor, CencEncryptor};
use crate::drm::scheme::EncryptionScheme;
use crate::error::Result;

/// Trait for decrypting individual media samples.
///
/// Implementations exist for both CBCS (AES-128-CBC pattern) and CENC (AES-128-CTR).
pub trait SampleDecryptor {
    /// Decrypt a single sample in place.
    ///
    /// * `data` — encrypted sample data (modified in place)
    /// * `iv` — initialization vector for this sample (size depends on scheme)
    /// * `subsamples` — optional subsample map (clear_bytes, encrypted_bytes) pairs
    fn decrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()>;
}

/// Trait for encrypting individual media samples.
///
/// Implementations exist for both CBCS (AES-128-CBC pattern) and CENC (AES-128-CTR).
pub trait SampleEncryptor {
    /// Encrypt a single sample in place.
    ///
    /// * `data` — plaintext sample data (modified in place)
    /// * `iv` — initialization vector for this sample (size depends on scheme)
    /// * `subsamples` — optional subsample map (clear_bytes, encrypted_bytes) pairs
    fn encrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()>;

    /// Generate a per-sample IV for this encryption scheme.
    ///
    /// * `segment_number` — index of the current segment
    /// * `sample_index` — index of the sample within the segment
    fn generate_iv(&self, segment_number: u32, sample_index: u32) -> Vec<u8>;
}

// --- SampleDecryptor implementations ---

impl SampleDecryptor for CbcsDecryptor {
    fn decrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()> {
        CbcsDecryptor::decrypt_sample(self, data, iv, subsamples)
    }
}

impl SampleDecryptor for CencDecryptor {
    fn decrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()> {
        CencDecryptor::decrypt_sample(self, data, iv, subsamples)
    }
}

// --- SampleEncryptor implementations ---

impl SampleEncryptor for CbcsEncryptor {
    fn encrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()> {
        CbcsEncryptor::encrypt_sample(self, data, iv, subsamples)
    }

    fn generate_iv(&self, segment_number: u32, sample_index: u32) -> Vec<u8> {
        CbcsEncryptor::generate_iv(segment_number, sample_index).to_vec()
    }
}

impl SampleEncryptor for CencEncryptor {
    fn encrypt_sample(
        &self,
        data: &mut [u8],
        iv: &[u8],
        subsamples: Option<&[(u32, u32)]>,
    ) -> Result<()> {
        CencEncryptor::encrypt_sample(self, data, iv, subsamples)
    }

    fn generate_iv(&self, segment_number: u32, sample_index: u32) -> Vec<u8> {
        cenc::generate_sample_iv(segment_number, sample_index).to_vec()
    }
}

/// Create a decryptor for the given encryption scheme.
///
/// * `scheme` — the source encryption scheme
/// * `key` — 16-byte AES content key
/// * `pattern` — (crypt_byte_block, skip_byte_block) for CBCS; ignored for CENC
pub fn create_decryptor(
    scheme: EncryptionScheme,
    key: [u8; 16],
    pattern: (u8, u8),
) -> Box<dyn SampleDecryptor> {
    match scheme {
        EncryptionScheme::Cbcs => {
            Box::new(CbcsDecryptor::new(key, pattern.0, pattern.1))
        }
        EncryptionScheme::Cenc => {
            Box::new(CencDecryptor::new(key))
        }
        EncryptionScheme::None => {
            panic!("create_decryptor called with EncryptionScheme::None; clear content should skip decryption")
        }
    }
}

/// Create an encryptor for the given encryption scheme.
///
/// * `scheme` — the target encryption scheme
/// * `key` — 16-byte AES content key
/// * `pattern` — (crypt_byte_block, skip_byte_block) for CBCS; ignored for CENC
pub fn create_encryptor(
    scheme: EncryptionScheme,
    key: [u8; 16],
    pattern: (u8, u8),
) -> Box<dyn SampleEncryptor> {
    match scheme {
        EncryptionScheme::Cbcs => {
            Box::new(CbcsEncryptor::new(key, pattern.0, pattern.1))
        }
        EncryptionScheme::Cenc => {
            Box::new(CencEncryptor::new(key))
        }
        EncryptionScheme::None => {
            panic!("create_encryptor called with EncryptionScheme::None; clear content should skip encryption")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_cbcs_decryptor() {
        let dec = create_decryptor(EncryptionScheme::Cbcs, [0x42; 16], (1, 9));
        // Should work without error
        let mut data = [0u8; 15]; // smaller than block, noop
        dec.decrypt_sample(&mut data, &[0u8; 16], None).unwrap();
    }

    #[test]
    fn create_cenc_decryptor() {
        let dec = create_decryptor(EncryptionScheme::Cenc, [0x42; 16], (0, 0));
        let mut data = vec![];
        dec.decrypt_sample(&mut data, &[0u8; 8], None).unwrap();
    }

    #[test]
    fn create_cbcs_encryptor() {
        let enc = create_encryptor(EncryptionScheme::Cbcs, [0x42; 16], (1, 9));
        let iv = enc.generate_iv(0, 0);
        assert_eq!(iv.len(), 16); // CBCS IVs are 16 bytes
    }

    #[test]
    fn create_cenc_encryptor() {
        let enc = create_encryptor(EncryptionScheme::Cenc, [0x42; 16], (0, 0));
        let iv = enc.generate_iv(0, 0);
        assert_eq!(iv.len(), 8); // CENC IVs are 8 bytes
    }

    #[test]
    fn cbcs_roundtrip_via_traits() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 16];
        let plaintext = vec![0xDE; 48];

        let enc = create_encryptor(EncryptionScheme::Cbcs, key, (0, 0));
        let dec = create_decryptor(EncryptionScheme::Cbcs, key, (0, 0));

        let mut data = plaintext.clone();
        enc.encrypt_sample(&mut data, &iv, None).unwrap();
        assert_ne!(data, plaintext);

        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn cenc_roundtrip_via_traits() {
        let key = [0x42u8; 16];
        let iv = [0x00u8; 8];
        let plaintext = vec![0xDE; 64];

        let enc = create_encryptor(EncryptionScheme::Cenc, key, (0, 0));
        let dec = create_decryptor(EncryptionScheme::Cenc, key, (0, 0));

        let mut data = plaintext.clone();
        enc.encrypt_sample(&mut data, &iv, None).unwrap();
        assert_ne!(data, plaintext);

        dec.decrypt_sample(&mut data, &iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn cross_scheme_cbcs_to_cenc() {
        let key = [0x42u8; 16];
        let cbcs_iv = [0x00u8; 16];
        let cenc_iv = [0x00u8; 8];
        let plaintext = vec![0xAA; 48];

        // Encrypt with CBCS (full pattern)
        let cbcs_enc = create_encryptor(EncryptionScheme::Cbcs, key, (0, 0));
        let mut data = plaintext.clone();
        cbcs_enc.encrypt_sample(&mut data, &cbcs_iv, None).unwrap();

        // Decrypt with CBCS
        let cbcs_dec = create_decryptor(EncryptionScheme::Cbcs, key, (0, 0));
        cbcs_dec.decrypt_sample(&mut data, &cbcs_iv, None).unwrap();
        assert_eq!(data, plaintext);

        // Re-encrypt with CENC
        let cenc_enc = create_encryptor(EncryptionScheme::Cenc, key, (0, 0));
        cenc_enc.encrypt_sample(&mut data, &cenc_iv, None).unwrap();
        assert_ne!(data, plaintext);

        // Decrypt with CENC
        let cenc_dec = create_decryptor(EncryptionScheme::Cenc, key, (0, 0));
        cenc_dec.decrypt_sample(&mut data, &cenc_iv, None).unwrap();
        assert_eq!(data, plaintext);
    }

    #[test]
    fn generate_iv_sizes_match_scheme() {
        let cbcs_enc = create_encryptor(EncryptionScheme::Cbcs, [0; 16], (1, 9));
        let cenc_enc = create_encryptor(EncryptionScheme::Cenc, [0; 16], (0, 0));

        for seg in 0..3 {
            for sample in 0..5 {
                let cbcs_iv = cbcs_enc.generate_iv(seg, sample);
                let cenc_iv = cenc_enc.generate_iv(seg, sample);
                assert_eq!(cbcs_iv.len(), 16);
                assert_eq!(cenc_iv.len(), 8);
            }
        }
    }
}
