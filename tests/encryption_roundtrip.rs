//! Integration tests: CBCS → plaintext → CENC encryption roundtrip.
//!
//! These tests verify the full encryption lifecycle:
//! 1. Encrypt plaintext with CBCS (CBC mode with pattern)
//! 2. Decrypt CBCS ciphertext back to plaintext
//! 3. Re-encrypt plaintext with CENC (CTR mode)
//! 4. Decrypt CENC ciphertext back to verify it matches original plaintext
//!
//! This is the core cryptographic operation of the edgepack.

mod common;

use edgepack::drm::cbcs::CbcsDecryptor;
use edgepack::drm::cenc::{self, CencEncryptor};

/// Helper: encrypt data with AES-128-CBC for creating CBCS test ciphertext.
fn cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &mut [u8]) {
    use aes::Aes128;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    let encryptor = Aes128CbcEnc::new(key.into(), iv.into());
    encryptor
        .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(data, data.len())
        .unwrap();
}

// ─── Full Roundtrip Tests ───────────────────────────────────────────

#[test]
fn cbcs_decrypt_then_cenc_encrypt_full_sample() {
    let source_key = common::TEST_SOURCE_KEY;
    let target_key = common::TEST_TARGET_KEY;
    let iv = common::TEST_IV;

    // Step 1: Create known plaintext (4 blocks = 64 bytes)
    let mut plaintext = vec![0u8; 64];
    for (i, b) in plaintext.iter_mut().enumerate() {
        *b = (i * 7 + 3) as u8; // Deterministic pattern
    }
    let original_plaintext = plaintext.clone();

    // Step 2: Encrypt with CBCS (full encryption, pattern 0:0)
    let mut cbcs_ciphertext = plaintext.clone();
    cbc_encrypt(&source_key, &iv, &mut cbcs_ciphertext);
    assert_ne!(cbcs_ciphertext, original_plaintext, "CBCS encryption should change data");

    // Step 3: Decrypt CBCS
    let cbcs_dec = CbcsDecryptor::new(source_key, 0, 0);
    cbcs_dec
        .decrypt_sample(&mut cbcs_ciphertext, &iv, None)
        .expect("CBCS decryption should succeed");
    assert_eq!(
        cbcs_ciphertext, original_plaintext,
        "CBCS decryption should recover original plaintext"
    );

    // Step 4: Re-encrypt with CENC
    let cenc_iv = cenc::generate_sample_iv(0, 0);
    let cenc_enc = CencEncryptor::new(target_key);
    let mut cenc_ciphertext = cbcs_ciphertext; // now contains plaintext
    cenc_enc
        .encrypt_sample(&mut cenc_ciphertext, &cenc_iv, None)
        .expect("CENC encryption should succeed");
    assert_ne!(
        cenc_ciphertext, original_plaintext,
        "CENC encryption should change data"
    );

    // Step 5: Decrypt CENC (CTR is symmetric)
    let mut decrypted = cenc_ciphertext;
    cenc_enc
        .encrypt_sample(&mut decrypted, &cenc_iv, None)
        .expect("CENC decryption should succeed");
    assert_eq!(
        decrypted, original_plaintext,
        "CENC decrypt should recover original plaintext"
    );
}

#[test]
fn cbcs_1_9_pattern_then_cenc_roundtrip() {
    let source_key = common::TEST_SOURCE_KEY;
    let target_key = common::TEST_TARGET_KEY;
    let iv = common::TEST_IV;

    // 20 blocks = 320 bytes — enough for two full 1:9 pattern cycles
    let mut plaintext = vec![0u8; 320];
    for (i, b) in plaintext.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    let original_plaintext = plaintext.clone();

    // Encrypt with CBCS pattern 1:9 (encrypt block 0, skip blocks 1-9, repeat)
    let mut cbcs_ciphertext = plaintext.clone();
    // Encrypt block 0 (bytes 0..16)
    cbc_encrypt(&source_key, &iv, &mut cbcs_ciphertext[0..16]);
    // Encrypt block 10 (bytes 160..176)
    cbc_encrypt(&source_key, &iv, &mut cbcs_ciphertext[160..176]);

    // Decrypt with CBCS decryptor (pattern 1:9)
    let cbcs_dec = CbcsDecryptor::new(source_key, 1, 9);
    cbcs_dec
        .decrypt_sample(&mut cbcs_ciphertext, &iv, None)
        .expect("CBCS 1:9 decryption should succeed");
    assert_eq!(
        cbcs_ciphertext, original_plaintext,
        "CBCS 1:9 decryption should recover original plaintext"
    );

    // Re-encrypt with CENC (full CTR)
    let cenc_iv = cenc::generate_sample_iv(1, 0);
    let cenc_enc = CencEncryptor::new(target_key);
    let mut cenc_ciphertext = cbcs_ciphertext;
    cenc_enc
        .encrypt_sample(&mut cenc_ciphertext, &cenc_iv, None)
        .expect("CENC encryption should succeed");

    // Verify CENC roundtrip
    let mut final_plaintext = cenc_ciphertext;
    cenc_enc
        .encrypt_sample(&mut final_plaintext, &cenc_iv, None)
        .expect("CENC decryption should succeed");
    assert_eq!(
        final_plaintext, original_plaintext,
        "Full CBCS→CENC roundtrip should preserve plaintext"
    );
}

#[test]
fn cbcs_decrypt_cenc_encrypt_with_subsamples() {
    let source_key = common::TEST_SOURCE_KEY;
    let target_key = common::TEST_TARGET_KEY;
    let iv = common::TEST_IV;

    // Simulate a video NAL unit: 10 clear header bytes + 48 encrypted bytes
    let total_size = 58;
    let mut plaintext = vec![0u8; total_size];
    for (i, b) in plaintext.iter_mut().enumerate() {
        *b = (i * 13 + 5) as u8;
    }
    let original_plaintext = plaintext.clone();

    // Encrypt the encrypted portion (bytes 10..58 = 48 bytes = 3 blocks) with CBCS
    let mut cbcs_ciphertext = plaintext.clone();
    cbc_encrypt(&source_key, &iv, &mut cbcs_ciphertext[10..58]);

    // Verify clear bytes are unchanged
    assert_eq!(
        &cbcs_ciphertext[..10],
        &original_plaintext[..10],
        "Clear bytes should be unchanged"
    );

    // Decrypt CBCS with subsample mapping
    let subsamples = [(10u32, 48u32)];
    let cbcs_dec = CbcsDecryptor::new(source_key, 0, 0);
    cbcs_dec
        .decrypt_sample(&mut cbcs_ciphertext, &iv, Some(&subsamples))
        .expect("CBCS subsample decryption should succeed");
    assert_eq!(
        cbcs_ciphertext, original_plaintext,
        "CBCS subsample decryption should recover original"
    );

    // Re-encrypt with CENC using subsamples
    let cenc_iv = cenc::generate_sample_iv(0, 0);
    let cenc_enc = CencEncryptor::new(target_key);
    let mut cenc_ciphertext = cbcs_ciphertext;
    cenc_enc
        .encrypt_sample(&mut cenc_ciphertext, &cenc_iv, Some(&subsamples))
        .expect("CENC subsample encryption should succeed");

    // Clear bytes should still be unchanged
    assert_eq!(
        &cenc_ciphertext[..10],
        &original_plaintext[..10],
        "Clear bytes should remain unchanged after CENC"
    );

    // Encrypted portion should differ
    assert_ne!(
        &cenc_ciphertext[10..58],
        &original_plaintext[10..58],
        "Encrypted portion should be changed by CENC"
    );

    // Verify full roundtrip via CENC decrypt
    let mut final_plaintext = cenc_ciphertext;
    cenc_enc
        .encrypt_sample(&mut final_plaintext, &cenc_iv, Some(&subsamples))
        .expect("CENC subsample decryption should succeed");
    assert_eq!(
        final_plaintext, original_plaintext,
        "Full subsample roundtrip should preserve plaintext"
    );
}

#[test]
fn multiple_samples_unique_cenc_ivs() {
    let target_key = common::TEST_TARGET_KEY;
    let cenc_enc = CencEncryptor::new(target_key);

    let plaintext = vec![0xAA; 32]; // 2 blocks
    let mut ciphertexts = Vec::new();

    // Encrypt 10 samples with unique IVs
    for sample_idx in 0..10u32 {
        let iv = cenc::generate_sample_iv(0, sample_idx);
        let mut ct = plaintext.clone();
        cenc_enc.encrypt_sample(&mut ct, &iv, None).unwrap();
        ciphertexts.push(ct);
    }

    // Verify all ciphertexts are different
    for i in 0..ciphertexts.len() {
        for j in (i + 1)..ciphertexts.len() {
            assert_ne!(
                ciphertexts[i], ciphertexts[j],
                "Samples {i} and {j} should have different ciphertexts due to unique IVs"
            );
        }
    }
}

#[test]
fn different_segments_produce_different_ivs() {
    // Verify IVs are unique across segments
    let mut ivs = Vec::new();
    for seg in 0..5u32 {
        for sample in 0..3u32 {
            ivs.push(cenc::generate_sample_iv(seg, sample));
        }
    }

    for i in 0..ivs.len() {
        for j in (i + 1)..ivs.len() {
            assert_ne!(
                ivs[i], ivs[j],
                "IVs should be unique across all segment/sample combinations"
            );
        }
    }
}

#[test]
fn roundtrip_with_different_source_and_target_keys() {
    let source_key = common::TEST_SOURCE_KEY;
    let target_key = common::TEST_TARGET_KEY;

    // Ensure source and target keys are different
    assert_ne!(source_key, target_key, "Test requires different keys");

    let iv = common::TEST_IV;
    let plaintext = vec![0xBB; 48]; // 3 blocks
    let original = plaintext.clone();

    // Encrypt with source key (CBCS)
    let mut data = plaintext;
    cbc_encrypt(&source_key, &iv, &mut data);

    // Decrypt with source key (CBCS)
    let dec = CbcsDecryptor::new(source_key, 0, 0);
    dec.decrypt_sample(&mut data, &iv, None).unwrap();
    assert_eq!(data, original);

    // Re-encrypt with target key (CENC)
    let cenc_iv = cenc::generate_sample_iv(0, 0);
    let enc = CencEncryptor::new(target_key);
    enc.encrypt_sample(&mut data, &cenc_iv, None).unwrap();

    // Trying to decrypt with source key should NOT recover original
    let wrong_dec = CencEncryptor::new(source_key);
    let mut wrong_decrypt = data.clone();
    wrong_dec
        .encrypt_sample(&mut wrong_decrypt, &cenc_iv, None)
        .unwrap();
    assert_ne!(
        wrong_decrypt, original,
        "Decrypting with wrong key should not recover plaintext"
    );

    // Decrypting with correct target key DOES recover original
    let correct_dec = CencEncryptor::new(target_key);
    correct_dec
        .encrypt_sample(&mut data, &cenc_iv, None)
        .unwrap();
    assert_eq!(
        data, original,
        "Decrypting with correct key should recover plaintext"
    );
}

#[test]
fn audio_sample_full_encryption_roundtrip() {
    // Audio uses full encryption (pattern 0:0) — no subsamples
    let source_key = common::TEST_SOURCE_KEY;
    let target_key = common::TEST_TARGET_KEY;
    let iv = common::TEST_IV;

    // Simulate an AAC audio frame (1024 samples × 2 bytes = 2048 bytes)
    let mut audio_frame = vec![0u8; 2048];
    for (i, b) in audio_frame.iter_mut().enumerate() {
        *b = ((i * 3 + 17) & 0xFF) as u8;
    }
    let original = audio_frame.clone();

    // Encrypt with CBCS (full encryption — 0:0 pattern, typical for audio)
    let mut encrypted = audio_frame;
    let block_end = (2048 / 16) * 16; // 2048 is perfectly aligned
    cbc_encrypt(&source_key, &iv, &mut encrypted[..block_end]);

    // Decrypt CBCS
    let dec = CbcsDecryptor::new(source_key, 0, 0);
    dec.decrypt_sample(&mut encrypted, &iv, None).unwrap();
    assert_eq!(encrypted, original, "Audio CBCS decrypt should recover original");

    // Re-encrypt with CENC (also full encryption — no subsamples for audio)
    let cenc_iv = cenc::generate_sample_iv(0, 0);
    let enc = CencEncryptor::new(target_key);
    enc.encrypt_sample(&mut encrypted, &cenc_iv, None).unwrap();

    // Verify CENC roundtrip
    let mut final_data = encrypted;
    enc.encrypt_sample(&mut final_data, &cenc_iv, None).unwrap();
    assert_eq!(final_data, original, "Audio CENC roundtrip should preserve data");
}

#[test]
fn video_sample_multiple_subsamples_roundtrip() {
    let source_key = common::TEST_SOURCE_KEY;
    let target_key = common::TEST_TARGET_KEY;
    let iv = common::TEST_IV;

    // Simulate a video sample with multiple NAL units:
    // NAL 1: 5 clear (header) + 32 encrypted
    // NAL 2: 3 clear (header) + 48 encrypted
    // Total: 88 bytes
    let total = 88;
    let mut plaintext = vec![0u8; total];
    for (i, b) in plaintext.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    let original = plaintext.clone();

    // Encrypt the encrypted portions with CBCS
    let mut data = plaintext;
    cbc_encrypt(&source_key, &iv, &mut data[5..37]); // NAL 1 encrypted portion (32 bytes)
    cbc_encrypt(&source_key, &iv, &mut data[40..88]); // NAL 2 encrypted portion (48 bytes)

    // Decrypt with CBCS using subsamples
    let subsamples = [(5u32, 32u32), (3u32, 48u32)];
    let dec = CbcsDecryptor::new(source_key, 0, 0);
    dec.decrypt_sample(&mut data, &iv, Some(&subsamples))
        .unwrap();
    assert_eq!(data, original, "Multi-subsample CBCS decrypt should work");

    // Re-encrypt with CENC
    let cenc_iv = cenc::generate_sample_iv(0, 0);
    let enc = CencEncryptor::new(target_key);
    enc.encrypt_sample(&mut data, &cenc_iv, Some(&subsamples))
        .unwrap();

    // Verify clear portions unchanged
    assert_eq!(&data[..5], &original[..5], "NAL 1 header should be clear");
    assert_eq!(
        &data[37..40],
        &original[37..40],
        "NAL 2 header should be clear"
    );

    // Verify CENC roundtrip
    let mut final_data = data;
    enc.encrypt_sample(&mut final_data, &cenc_iv, Some(&subsamples))
        .unwrap();
    assert_eq!(
        final_data, original,
        "Multi-subsample CENC roundtrip should preserve data"
    );
}
