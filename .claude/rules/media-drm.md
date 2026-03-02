---
paths:
  - "src/drm/**/*.rs"
  - "src/media/**/*.rs"
---

# Media & DRM

- ISOBMFF box types are defined in `media::box_type` constants. Use those, don't hardcode FourCC values.
- Encryption transforms have four dispatch paths based on (source_encrypted, target_encrypted). All four must be handled.
- CBCS uses AES-128-CBC with pattern encryption (1:9 video, 0:0 audio), 16-byte IVs.
- CENC uses AES-128-CTR with full encryption, 8-byte IVs.
- FairPlay PSSH boxes are excluded from CENC output (FairPlay doesn't support CENC).
- Multi-key content uses `TrackKeyMapping` for per-track KIDs — always thread it through the pipeline.
- PSSH boxes are grouped by system_id with all unique KIDs per system (v1 format).
