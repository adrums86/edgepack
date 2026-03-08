# edgepack Architecture

All diagrams use [Mermaid](https://mermaid.js.org/) syntax (11 diagrams total). They render natively in Confluence (Mermaid macro), Jira (Mermaid code blocks), GitHub, and can be imported into Lucidchart via **File → Import → Mermaid**.

---

## 1. System Context

Shows how the edgepack WASM module fits into the CDN infrastructure and its external dependencies.

```mermaid
graph TB
    subgraph Client["Client Device"]
        Player["Video Player<br/>(browser / app)"]
    end

    subgraph CDN["CDN Edge Network"]
        Cache["CDN Cache Layer<br/>HTTP Cache-Control headers"]
        subgraph WASM["edgepack.wasm"]
            Handler["HTTP Handler<br/>(wasi:http/incoming-handler)"]
            Pipeline["Repackage Pipeline"]
            MediaEngine["Media Engine<br/>(ISOBMFF/CMAF)"]
            DRM["DRM Module<br/>(SPEKE 2.0 / CPIX)"]
            ManifestGen["Manifest Generator<br/>(HLS / DASH)"]
            CacheMod["Cache Module<br/>(AES-128-CTR encrypted)"]
        end
    end

    Origin["Origin Server<br/>CBCS/CENC-encrypted<br/>CMAF content"]
    LicenseServer["DRM License Server<br/>SPEKE 2.0 endpoint"]

    Player -- "GET /repackage/{id}/{fmt}/..." --> Cache
    Cache -- "cache miss" --> Handler
    Handler --> Pipeline
    Pipeline --> MediaEngine
    Pipeline --> DRM
    Pipeline --> ManifestGen
    Pipeline --> CacheMod
    DRM -- "POST CPIX XML" --> LicenseServer
    LicenseServer -- "content keys +<br/>PSSH data" --> DRM
    MediaEngine -- "GET init/segments" --> Origin
    Origin -- "CBCS/CENC segments" --> MediaEngine
    Cache -- "repackaged segments +<br/>manifest" --> Player

    classDef external fill:#374151,stroke:#6B7280,color:#F9FAFB
    classDef cdn fill:#1E3A5F,stroke:#3B82F6,color:#F9FAFB
    classDef wasm fill:#1E3A5F,stroke:#6366F1,color:#F9FAFB
    classDef client fill:#1F2937,stroke:#10B981,color:#F9FAFB

    class Player client
    class Cache cdn
    class Handler,Pipeline,MediaEngine,DRM,ManifestGen,CacheMod wasm
    class Origin,LicenseServer external
```

---

## 2. Repackaging Data Flow

Shows the complete data transformation pipeline with configurable source/target encryption schemes and output container format.

```mermaid
flowchart LR
    subgraph Input["INPUT (CBCS or CENC)"]
        SrcManifest["Source Manifest<br/>.m3u8 / .mpd<br/>(scheme auto-detected)"]
        SrcInit["Init Segment<br/>(sinf/schm/tenc<br/>+ DRM system PSSHs)"]
        SrcSeg["Media Segments<br/>(source-scheme<br/>encrypted mdat)"]
    end

    subgraph Transform["TRANSFORM"]
        Parse["Parse Source<br/>Manifest +<br/>detect scheme"]
        FetchKeys["Fetch Keys<br/>via SPEKE 2.0<br/>(multi-KID CPIX)"]
        ParseInit["Parse Init<br/>Protection Info +<br/>extract tracks<br/>(codec strings,<br/>per-track KIDs)"]
        RewriteInit["Rewrite Init<br/>schm→target scheme<br/>tenc→per-track KIDs<br/>ftyp→target format<br/>multi-KID PSSH<br/>(±FairPlay)"]
        Decrypt["Decrypt mdat<br/>via create_decryptor()<br/>(CBCS or CENC)"]
        Encrypt["Re-encrypt mdat<br/>via create_encryptor()<br/>(CBCS or CENC)"]
        RewriteSenc["Rewrite senc<br/>new IVs<br/>(8B or 16B)"]
    end

    subgraph Output["OUTPUT (target scheme + format)"]
        OutManifest["Output Manifest<br/>(progressive:<br/>live → complete)<br/>scheme-aware DRM<br/>format-aware profiles"]
        OutInit["Init Segment<br/>(sinf/schm=target<br/>ftyp=target format<br/>scheme-filtered PSSHs)"]
        OutSeg["Media Segments<br/>(target-scheme encrypted<br/>.cmfv or .m4s ext)"]
    end

    SrcManifest --> Parse
    Parse --> FetchKeys
    SrcInit --> ParseInit
    ParseInit --> FetchKeys
    FetchKeys --> RewriteInit
    SrcInit --> RewriteInit
    RewriteInit --> OutInit
    SrcSeg --> Decrypt
    FetchKeys --> Decrypt
    Decrypt --> Encrypt
    Encrypt --> RewriteSenc
    RewriteSenc --> OutSeg
    OutInit --> OutManifest
    OutSeg --> OutManifest
```

---

## 3. Module Architecture

Shows the internal Rust module structure and dependency relationships.

```mermaid
graph TD
    subgraph Entry["Entry Points"]
        WASI["wasi_handler.rs<br/>(wasm32 only)"]
        Sandbox["bin/sandbox.rs<br/>(native only)"]
    end

    subgraph Handler["handler/"]
        Router["mod.rs<br/>route() dispatcher<br/>HttpRequest/Response"]
        Request["request.rs<br/>GET handlers"]
        Webhook["webhook.rs<br/>POST /config/source<br/>JIT source config"]
    end

    subgraph Repackager["repackager/"]
        RPipeline["pipeline.rs<br/>RepackagePipeline<br/>execute() + jit_setup()<br/>+ jit_segment()"]
        Progressive["progressive.rs<br/>ProgressiveOutput<br/>state machine"]
    end

    subgraph Core["Core Modules"]
        Media["media/<br/>ISOBMFF parser<br/>ContainerFormat<br/>codec + language extraction<br/>chunk boundary detection<br/>TS demux + transmux (ts feature)<br/>TS mux (ts feature)<br/>init rewrite (ftyp+sinf)<br/>segment rewrite"]
        DRMMod["drm/<br/>EncryptionScheme<br/>SampleDecryptor/Encryptor<br/>SPEKE client + CPIX XML<br/>CBCS decrypt+encrypt<br/>CENC encrypt+decrypt<br/>ClearKey PSSH builder"]
        Manifest["manifest/<br/>HLS renderer (incl. LL-HLS)<br/>DASH renderer (incl. LL-DASH)<br/>HLS input parser<br/>DASH input parser<br/>Subtitle/CEA signaling"]
        CacheMod2["cache/<br/>CacheBackend trait<br/>EncryptedCacheBackend<br/>(AES-128-CTR)<br/>InMemoryCacheBackend"]
    end

    subgraph Shared["Shared"]
        Config["config.rs<br/>AppConfig"]
        Error["error.rs<br/>EdgepackError"]
        HTTP["http_client.rs<br/>WASI / reqwest / stub"]
    end

    WASI --> Router
    Sandbox --> RPipeline

    Router --> Request
    Router --> Webhook
    Request --> CacheMod2
    Request --> Manifest
    Webhook --> RPipeline

    RPipeline --> Media
    RPipeline --> DRMMod
    RPipeline --> Progressive
    RPipeline --> CacheMod2
    Progressive --> Manifest

    Media --> HTTP
    DRMMod --> HTTP

    RPipeline --> Config
    RPipeline --> Error
    Router --> Config
```

---

## 4. JIT Execution Model

Shows how edgepack handles on-demand JIT packaging when content is requested for the first time.

```mermaid
sequenceDiagram
    participant Client
    participant CDN as CDN Cache
    participant EP as edgepack
    participant Origin
    participant SPEKE as License Server

    Note over Client,SPEKE: Cache Miss — JIT Setup + Segment Processing
    Client->>CDN: GET /repackage/{id}/hls_cenc/manifest
    CDN-->>EP: cache miss → instantiate WASM
    EP->>Origin: GET source manifest
    Origin-->>EP: .m3u8 / .mpd
    EP->>Origin: GET init segment
    Origin-->>EP: init.mp4 (source scheme)
    EP->>SPEKE: POST CPIX request
    SPEKE-->>EP: content keys + PSSH data
    EP->>EP: Rewrite init (source→target scheme)
    EP->>EP: Cache keys + rewrite params (AES-128-CTR encrypted)

    loop For each segment
        EP->>Origin: GET segment_N
        Origin-->>EP: segment_N (source scheme)
        EP->>EP: Decrypt (source) → Re-encrypt (target)
        EP->>EP: Update manifest state
    end

    EP->>EP: Finalize manifest
    EP->>EP: cleanup_sensitive_data() — delete keys + SPEKE response
    EP-->>CDN: manifest + segments (Cache-Control: immutable)
    CDN-->>Client: manifest.m3u8

    Note over Client,SPEKE: Subsequent Requests — CDN Cache Hit
    Client->>CDN: GET /repackage/{id}/hls_cenc/segment_0.cmfv
    CDN-->>Client: segment (Cache-Control: immutable, 1yr)
```

---

## 5. Progressive Output State Machine

Shows the manifest lifecycle phases and transitions.

```mermaid
stateDiagram-v2
    [*] --> AwaitingFirstSegment: Pipeline starts

    AwaitingFirstSegment --> Live: First segment + init complete
    note right of AwaitingFirstSegment
        No manifest available yet.
        No content to serve.
    end note

    Live --> Live: Each subsequent segment
    note right of Live
        Manifest includes all segments so far.
        Cache-Control: max-age=1, s-maxage=1
        HLS: no #EXT-X-ENDLIST
        DASH: type="dynamic"
    end note

    Live --> Complete: Final segment processed
    note right of Complete
        Full manifest with all segments.
        Cache-Control: max-age=31536000, immutable
        HLS: #EXT-X-ENDLIST added
        DASH: type="static"
        Sensitive cache entries cleaned up.
    end note

    Complete --> [*]
```

---

## 6. Cache Security Model

Shows the encryption-at-rest and cleanup approach for sensitive DRM data in the in-process cache.

```mermaid
flowchart TB
    subgraph Pipeline["RepackagePipeline"]
        Execute["execute() /<br/>jit_setup() /<br/>jit_segment()"]
        Cleanup["cleanup_sensitive_data()<br/>called after execute() +<br/>jit_setup() complete"]
    end

    subgraph EncLayer["EncryptedCacheBackend (decorator)"]
        Check{"is_sensitive_key?<br/>(:keys, :speke,<br/>:rewrite_params)"}
        AES["AES-128-CTR<br/>Encrypt/Decrypt"]
        Pass["Pass-through<br/>(no encryption)"]
    end

    subgraph Inner["InMemoryCacheBackend"]
        Memory["Arc&lt;RwLock&lt;HashMap&gt;&gt;<br/>(per-process singleton)"]
    end

    subgraph KeyGen["Key Generation"]
        Entropy["Process entropy<br/>(pointer addresses +<br/>AES whitening)"]
        Key["Per-process<br/>128-bit key"]
    end

    Execute -- "set(key, value)" --> Check
    Check -- "Yes (sensitive)" --> AES
    Check -- "No" --> Pass
    AES -- "iv ‖ ciphertext" --> Memory
    Pass -- "plaintext" --> Memory

    Entropy --> Key
    Key --> AES

    Execute --> Cleanup
    Cleanup -- "DELETE :keys" --> Memory
    Cleanup -- "DELETE :speke" --> Memory
```

---

## 7. Cache Key Layout

Shows all cache keys, their sensitivity classification, and lifecycle.

```mermaid
graph LR
    subgraph Sensitive["SENSITIVE (encrypted + deleted on completion)"]
        style Sensitive fill:#7F1D1D,stroke:#EF4444,color:#FCA5A5
        K1["ep:{id}:keys<br/>DRM content keys"]
        K2["ep:{id}:speke<br/>SPEKE CPIX response"]
        K3["ep:{id}:{fmt}:rewrite_params<br/>encryption keys + IVs"]
    end

    subgraph NonSensitive["NON-SENSITIVE (plaintext)"]
        style NonSensitive fill:#14532D,stroke:#22C55E,color:#BBF7D0
        K6["ep:{id}:{fmt}_{scheme}:manifest_state<br/>progressive manifest (per format)"]
        K7["ep:{id}:{scheme}:init<br/>rewritten init segment<br/>(format-agnostic)"]
        K8["ep:{id}:{scheme}:seg:{n}<br/>rewritten media segments<br/>(format-agnostic)"]
    end
```

---

## 8. CDN Caching Strategy

Shows how different resource types are cached at the CDN layer.

```mermaid
flowchart LR
    subgraph Resources["Resource Types"]
        Seg["Segments<br/>(init.mp4, segment_N.cmfv/.m4s)"]
        FinalManifest["Finalized Manifest<br/>(VOD / complete)"]
        LiveManifest["Live Manifest<br/>(in-progress)"]
        Health["Health Check<br/>(/health)"]
    end

    subgraph CachePolicy["Cache-Control Policy"]
        Immutable["public, max-age=31536000,<br/>immutable<br/>(1 year, never revalidate)"]
        ShortTTL["public, max-age=1,<br/>s-maxage=1<br/>(1 second, always revalidate)"]
        NoCache["no-cache"]
    end

    Seg --> Immutable
    FinalManifest --> Immutable
    LiveManifest --> ShortTTL
    Health --> NoCache
```

---

## 9. Encryption Transform Detail

Shows the per-segment encryption transform at the byte level. Source and target schemes are configurable — the pipeline uses `create_decryptor()` and `create_encryptor()` factory functions to dispatch to the correct scheme implementation.

```mermaid
flowchart TD
    subgraph SourceSegment["Source Segment (CBCS or CENC)"]
        MOOF1["moof box<br/>├ mfhd (sequence)<br/>└ traf<br/>   ├ tfhd<br/>   ├ tfdt<br/>   ├ trun (sample sizes)<br/>   └ senc (per-sample IVs<br/>      ± subsample map)"]
        MDAT1["mdat box<br/>(source-scheme<br/>encrypted)"]
    end

    subgraph Transform["Transform Steps"]
        S1["1. Parse senc → extract<br/>per-sample IVs +<br/>subsample ranges"]
        S2["2. Parse trun → extract<br/>per-sample byte sizes"]
        S3["3. create_decryptor(source_scheme)<br/>   For each sample in mdat:<br/>   Decrypt with source scheme"]
        S4["4. create_encryptor(target_scheme)<br/>   For each sample:<br/>   Generate target-scheme IV<br/>   Encrypt with target scheme"]
        S5["5. Rebuild senc with<br/>new IVs (8B for CENC,<br/>16B for CBCS)"]
        S6["6. Rebuild moof + mdat"]
    end

    subgraph OutputSegment["Output Segment (target scheme)"]
        MOOF2["moof box<br/>├ mfhd (sequence)<br/>└ traf<br/>   ├ tfhd<br/>   ├ tfdt<br/>   ├ trun (same sizes)<br/>   └ senc (new IVs<br/>      target IV size)"]
        MDAT2["mdat box<br/>(target-scheme<br/>encrypted)"]
    end

    MOOF1 --> S1
    MOOF1 --> S2
    MDAT1 --> S3
    S1 --> S3
    S2 --> S3
    S3 --> S4
    S4 --> S5
    S4 --> S6
    S5 --> MOOF2
    S6 --> MDAT2
```

---

## 11. I-Frame Detection & Trick Play Flow

Shows how I-frame byte ranges are detected from rewritten segments and rendered as trick play manifests.

```mermaid
flowchart TD
    subgraph Pipeline["Pipeline (per segment)"]
        Rewrite["Rewrite Segment<br/>(decrypt → re-encrypt)"]
        ChunkDetect["detect_chunk_boundaries()<br/>(media/chunk.rs)"]
        FindIDR["Find first independent chunk<br/>(is_independent_chunk)"]
        Record["Record IFrameSegmentInfo<br/>byte_offset, byte_length,<br/>duration, segment_uri"]
    end

    subgraph ManifestState["ManifestState"]
        IFrameSegs["iframe_segments:<br/>Vec&lt;IFrameSegmentInfo&gt;"]
        Enable["enable_iframe_playlist: true"]
    end

    subgraph HLS["HLS Output"]
        IFramePlaylist["I-Frame Playlist<br/>#EXT-X-I-FRAMES-ONLY<br/>#EXT-X-VERSION:4<br/>#EXT-X-BYTERANGE:len@off<br/>per segment"]
        MasterPlaylist["Master Playlist<br/>#EXT-X-I-FRAME-STREAM-INF<br/>BANDWIDTH, CODECS,<br/>RESOLUTION, URI=iframes"]
    end

    subgraph DASH["DASH Output"]
        TrickAS["Trick Play AdaptationSet<br/>EssentialProperty<br/>schemeIdUri=trickmode<br/>value=1 (main video id)"]
        MainAS["Main Video AdaptationSet<br/>id=1"]
    end

    subgraph Route["HTTP Route"]
        IFrameRoute["GET /repackage/{id}/{fmt}/iframes<br/>→ render_iframe_manifest()"]
    end

    Rewrite --> ChunkDetect
    ChunkDetect --> FindIDR
    FindIDR --> Record
    Record --> IFrameSegs
    Enable --> IFrameSegs

    IFrameSegs --> IFramePlaylist
    IFrameSegs --> MasterPlaylist
    IFrameSegs --> TrickAS
    IFrameSegs --> MainAS

    IFrameRoute --> IFramePlaylist
```

---

## Key Features Summary

| Feature | Description |
|---------|-------------|
| **Configurable Encryption** | Transforms between CBCS ↔ CENC in any direction; target scheme configurable per request |
| **Configurable Container Format** | Output as CMAF (`.cmfv`, `cmfc` brand), fMP4 (`.m4s`), or TS (`.ts`, HLS-only); ftyp rewriting, dynamic DASH profiles |
| **Source Scheme Auto-Detection** | Detects source encryption from init segment `schm` box or manifest DRM signaling |
| **Trait-Based Crypto Dispatch** | `SampleDecryptor`/`SampleEncryptor` traits with factory functions for scheme-agnostic pipeline |
| **Progressive Output** | Clients can begin playback as soon as the first segment is ready |
| **JIT Packaging** | On-demand GET packaging — manifest/init/segment on cache miss with <1 ms cold start |
| **Encryption at Rest** | Sensitive cache entries (DRM keys, SPEKE responses) encrypted with AES-128-CTR per-process key |
| **Immediate Cleanup** | All sensitive data deleted from cache the moment processing completes |
| **Aggressive CDN Caching** | Segments and finalized manifests cached for 1 year; live manifests refresh every second |
| **Multi-DRM** | Widevine + PlayReady for CENC output; FairPlay + Widevine + PlayReady for CBCS output |
| **Multi-Key DRM** | Per-track keying (separate video/audio KIDs), multi-KID PSSH v1 boxes, TrackKeyMapping |
| **Codec Awareness** | RFC 6381 codec string extraction from init segments for manifest signaling |
| **Subtitle Pass-Through** | WebVTT/TTML in fMP4, HLS subtitle rendition groups, DASH text AdaptationSets, CEA-608/708 caption signaling |
| **Sub-Millisecond Cold Start** | ~628 KB WASM binary instantiates in <1 ms, 50-500x faster than Lambda/Cloud Functions |
| **SCTE-35 Ad Break Signaling** | emsg box extraction, splice event parsing, HLS `#EXT-X-DATERANGE` and DASH `EventStream` output, source manifest ad marker roundtrip |
| **Compatibility Validation** | Pre-flight codec/scheme checks, HDR format detection, init/segment structure validation, conformance test suite |
| **Advanced DRM** | ClearKey DRM system, raw key mode (bypass SPEKE), key rotation at segment boundaries, clear lead (unencrypted lead-in segments), explicit DRM system selection per request |
| **LL-HLS** | Low-Latency HLS with partial segments (`#EXT-X-PART`), server control (`#EXT-X-SERVER-CONTROL`), CMAF chunk boundary detection, HLS version 9 |
| **LL-DASH** | Low-Latency DASH with `availabilityTimeOffset` and `availabilityTimeComplete` on `<SegmentTemplate>` |
| **MPEG-TS Input** | TS demuxer (PAT/PMT/PES, H.264/AAC), TS-to-CMAF transmuxer (Annex B→AVCC, init synthesis), AES-128 segment decryption. Feature-gated (`ts` feature) |
| **MPEG-TS Output** | CMAF-to-TS muxer (AVCC→Annex B, raw AAC→ADTS, PAT/PMT/PES packetization), AES-128-CBC whole-segment encryption, HLS-TS manifests (`METHOD=AES-128`, no `#EXT-X-MAP`), key delivery endpoint. Feature-gated (`ts` feature) |
| **Trick Play** | HLS `#EXT-X-I-FRAMES-ONLY` playlists with `#EXT-X-BYTERANGE` into existing segments, `#EXT-X-I-FRAME-STREAM-INF` in master. DASH trick play `<AdaptationSet>` with `<EssentialProperty>` trickmode. I-frame detection from CMAF chunk boundaries — no duplicate storage |
| **Dual-Format Output** | Simultaneous HLS + DASH from a single request sharing format-agnostic segments. `output_formats: ["hls", "dash"]` produces both manifest types referencing the same cached segments — no duplicate encryption or storage |
| **Binary Size Guards** | Tests enforce WASM binary size limits per build variant. Binary size is the primary cold start proxy |
| **Zero External Test Dependencies** | All 1,452 tests (1,290 without `ts` feature) use synthetic CMAF fixtures — no network or media files needed |
| **CDN-Portable WASM** | Entire runtime compiles to `wasm32-wasip2` — runs on any CDN with WASI P2 support (Cloudflare Workers, Fastly Compute, wasmtime on Lambda, Akamai EdgeCompute). No CDN-specific APIs, no vendor lock-in |

## Inputs and Outputs

| Direction | What | Format | Protocol |
|-----------|------|--------|----------|
| **Input** | Source manifest | HLS `.m3u8` or DASH `.mpd` (source scheme auto-detected) | HTTP GET from origin |
| **Input** | Source init segment | CMAF (CBCS or CENC sinf/schm/tenc/pssh) | HTTP GET from origin |
| **Input** | Source media segments | CMAF (source-scheme encrypted mdat) | HTTP GET from origin |
| **Input** | Source TS segments | MPEG-TS (H.264/AAC, optional AES-128 encryption) | HTTP GET from origin (`ts` feature) |
| **Input** | AES-128 key | Raw key bytes for HLS-TS segment decryption | HTTP GET from key URL (`ts` feature) |
| **Input** | DRM content keys | CPIX XML (SPEKE 2.0) | HTTP POST to license server |
| **Output** | Repackaged manifest | HLS `.m3u8` or DASH `.mpd` (target-scheme DRM signaling, format-aware profiles) | HTTP GET via CDN |
| **Output** | Repackaged init segment | CMAF or fMP4 (target-scheme schm/tenc/pssh, target-format ftyp brands, DRM systems per scheme) | HTTP GET via CDN |
| **Output** | Repackaged media segments | CMAF `.cmfv`, fMP4 `.m4s`, or TS `.ts` (target-scheme encrypted) | HTTP GET via CDN |

---

## Completed Architecture Extensions

### ~~Phase 2: Container Format Flexibility~~ ✅ Complete
- `ContainerFormat` enum (`Cmaf`, `Fmp4`, `Iso`) in `src/media/container.rs` with brand, extension, profile helpers
- ftyp box rewriting in init segments for output container format
- Dynamic segment extensions (`.cmfv`/`.cmfa` for CMAF, `.m4s` for fMP4, `.mp4` for ISO)
- Dynamic DASH profile signaling (`cmaf:2019` for CMAF, `isoff-live:2011` for fMP4/ISO)
- `container_format` flows through `RepackageRequest` → `ManifestState` → `ProgressiveOutput`
- Route handler accepts all 7 CMAF/ISOBMFF segment extensions

### ~~Phase 3: Unencrypted Input Support~~ ✅ Complete
- `EncryptionScheme::None` variant with `is_encrypted()` method
- Four-way init/segment dispatch (encrypted↔encrypted, clear→encrypted, encrypted→clear, clear→clear)
- `create_protection_info()` / `strip_protection_info()` / `rewrite_ftyp_only()` in init.rs
- Conditional SPEKE — skipped when both source and target are unencrypted

### ~~Phase 4: Dual-Scheme Output~~ ✅ Complete
- Multi-rendition pipeline: loop over target schemes, produce independent segment sets
- Scheme-qualified cache keys (`{format}_{scheme}` pattern, e.g. `hls_cenc`)
- Scheme-qualified URL routes (e.g. `/repackage/{id}/hls_cenc/manifest`)
- Source segments decrypted once, re-encrypted for each target scheme

### ~~Phase 5: Multi-Key DRM & Codec Awareness~~ ✅ Complete
- `TrackKeyMapping` type mapping `TrackType → [u8; 16]` KID for per-track keying
- Per-track `tenc` in init segments — video and audio tracks get different KIDs via `hdlr` detection
- Multi-KID PSSH v1 — grouped by `system_id`, all track KIDs embedded per DRM system
- Codec string extraction via `extract_tracks()` in `src/media/codec.rs` — RFC 6381 codec strings from stsd config boxes (avcC, hvcC, esds, vpcC, av1C, wvtt, stpp)
- Timescale and language parsing from `mdhd` box (ISO 639-2/T packed 3×5-bit chars)
- Pipeline integration: `extract_tracks()` → `build_track_key_mapping()` → multi-KID SPEKE → per-track init rewriting
- Codec strings populated into `VariantInfo` for HLS `CODECS=` and DASH `codecs=` manifest attributes

### ~~Phase 6: Subtitle & Text Track Pass-Through~~ ✅ Complete
- WebVTT (`wvtt`) and TTML (`stpp`) sample entry pass-through in fMP4 — subtitles bypass encryption via `encrypted_sample_entry_type()` returning `None`
- `TrackMediaType::Subtitle` enum variant, `language` field on `VariantInfo` and `TrackInfo`
- ISO 639-2/T language extraction from `mdhd` box (packed 3×5-bit chars)
- `CeaCaptionInfo` struct for CEA-608/708 manifest signaling (pass-through automatic in video SEI NALs)
- HLS subtitle rendition groups (`#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs"`) with `SUBTITLES="subs"` on `EXT-X-STREAM-INF`
- HLS CEA caption signaling (`#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,INSTREAM-ID=...`) with `CLOSED-CAPTIONS="cc"` on `EXT-X-STREAM-INF`
- DASH subtitle `<AdaptationSet contentType="text" mimeType="application/mp4">` with `lang` attribute
- DASH CEA `<Accessibility schemeIdUri="urn:scte:dash:cc:cea-608:2015">` descriptors inside video AdaptationSet

### ~~Phase 8: JIT Packaging (On-Demand GET)~~ ✅ Complete
- Manifest-on-GET, Init-on-GET, Segment-on-GET (lazy repackaging on cache miss)
- Request coalescing via `set_nx` distributed locking with configurable TTL
- `POST /config/source` endpoint for per-content source configuration
- URL pattern-based source resolution with `{content_id}` placeholder

### ~~Phase 9: LL-HLS & LL-DASH~~ ✅ Complete
- LL-HLS partial segments: `#EXT-X-PART`, `#EXT-X-PART-INF`, `#EXT-X-SERVER-CONTROL`, `#EXT-X-PRELOAD-HINT`
- LL-DASH: `availabilityTimeOffset` and `availabilityTimeComplete` on `<SegmentTemplate>`
- CMAF chunk boundary detection (`src/media/chunk.rs`) — finds moof+mdat pairs, checks independence via trun flags
- New types: `PartInfo`, `ServerControl`, `LowLatencyDashInfo`, `SourcePartInfo`
- Progressive output part support (`add_part()`, `part_data()`, LL setter methods)
- HLS version bump to 9 when LL-HLS parts present
- Source LL info threaded through pipeline to output manifests

### ~~Phase 10: MPEG-TS Input~~ ✅ Complete
- Feature-gated: all TS code behind `#[cfg(feature = "ts")]`
- TS demuxer (`src/media/ts.rs`): 188-byte packet parsing, PAT/PMT table parsing, PES reassembly, `TsDemuxer` stateful accumulator
- TS-to-CMAF transmuxer (`src/media/transmux.rs`): H.264 NAL extraction (Annex B→AVCC), SPS parsing, AAC ADTS config, init segment synthesis (ftyp+moov), moof+mdat fragment generation
- AES-128-CBC whole-segment decryption for HLS-TS (`decrypt_ts_segment()`)
- HLS input TS detection: `.ts` extension, `#EXT-X-KEY:METHOD=AES-128` with URI/IV, optional `#EXT-X-MAP`
- Pipeline integration: feature-gated `process_ts_segment()` — decrypt → demux → transmux → CMAF pipeline

### ~~Phase 11: Advanced DRM~~ ✅ Complete
- ClearKey DRM system ID (`e2719d58-a985-b3c9-781a-b030af78d30e`) with local PSSH builder (JSON `{"kids":["base64url"]}`)
- Raw key mode: accept encryption keys directly via webhook, bypass SPEKE
- Key rotation: per-period key rotation at configurable segment boundaries, new DRM signaling per period
- Clear lead: first N segments unencrypted, manifest transition at boundary
- DRM systems override: explicit selection of widevine/playready/fairplay/clearkey per request

### ~~Phase 12: Trick Play & I-Frame Playlists~~ ✅ Complete
- `IFrameSegmentInfo` type in `manifest/types.rs` — byte offset, length, duration, segment URI per I-frame
- `enable_iframe_playlist` opt-in field on `ManifestState` and `RepackageRequest` (default false, `#[serde(default)]` for backward compat)
- I-frame detection reuses `chunk.rs` — `detect_chunk_boundaries()` → find first independent (IDR) chunk → record byte range
- Consolidated chunk detection in pipeline — runs once when either LL-HLS parts or I-frame playlists need it
- HLS I-frame playlist: `#EXT-X-I-FRAMES-ONLY`, `#EXT-X-VERSION:4`, `#EXT-X-BYTERANGE:length@offset`, DRM KEY tags, init MAP
- HLS master playlist: `#EXT-X-I-FRAME-STREAM-INF` per video variant (bandwidth/10, codecs, resolution)
- DASH trick play: separate `<AdaptationSet>` with `<EssentialProperty schemeIdUri="http://dashif.org/guidelines/trickmode" value="1"/>` referencing main video by `id="1"`
- Dedicated route: `GET /repackage/{id}/{fmt}/iframes` (HLS only, DASH returns 404)
- Sandbox writes `iframes.m3u8` alongside regular HLS output

### ~~Phase 21: Generic HLS/DASH Pipeline (Dual-Format)~~ ✅ Complete
- `RepackageRequest.output_formats: Vec<OutputFormat>` replaces singular `output_format` — backward-compatible webhook API
- Format-agnostic segment cache keys: `ep:{id}:{scheme}:init` and `ep:{id}:{scheme}:seg:{n}` (no format prefix — segments are identical for HLS and DASH)
- Per-format manifest state: `ep:{id}:{format}_{scheme}:manifest_state` stays format-qualified (manifests differ per format)
- `execute()` returns `Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>` — one output per (format, scheme) pair
- Re-encryption runs once per scheme, then distributed to all output formats
- Combinatorial output: `output_formats: [Hls, Dash]` × `target_schemes: [Cenc, Cbcs]` = 4 outputs

### ~~Phase 22: MPEG-TS Output~~ ✅ Complete
- Feature-gated: behind existing `#[cfg(feature = "ts")]` gate (same as TS input)
- `ContainerFormat::Ts` variant — `.ts` extension, `is_isobmff()` returns false, no init segment, HLS-only (DASH+TS rejected)
- TS muxer (`src/media/ts_mux.rs`): CMAF moof/mdat → 188-byte TS packets (AVCC→Annex B, raw AAC→ADTS, PAT/PMT/PES)
- AES-128-CBC whole-segment encryption (`encrypt_ts_segment()` — reverse of Phase 10's `decrypt_ts_segment()`)
- HLS manifest: no `#EXT-X-MAP`, `#EXT-X-KEY:METHOD=AES-128,URI="{key_uri}"`, `#EXT-X-VERSION:3`, `.ts` segment URIs
- Key delivery endpoint: `GET /repackage/{id}/{format}/key` serves raw 16-byte AES key
- Pipeline: `TsMuxConfig` extracted from init segment, segments muxed via `mux_to_ts()`
- Validation: TS+DASH rejected, webhook accepts `"ts"` as container_format

## Planned Architecture Extensions

All P0 and P1 items are complete. Remaining phases are P2. See [`roadmap.md`](roadmap.md) for details.

---

## 10. Container Format Comparison

Shows the differences between CMAF and fMP4 output formats and how they flow through the system.

```mermaid
graph TB
    subgraph ContainerFormat["ContainerFormat Enum"]
        CMAF["CMAF<br/>(Common Media Application Format)"]
        FMP4["fMP4<br/>(Fragmented MP4)"]
        TS["TS<br/>(MPEG Transport Stream)<br/>(ts feature)"]
    end

    subgraph CMAF_Props["CMAF Properties"]
        style CMAF_Props fill:#1E3A5F,stroke:#3B82F6,color:#F9FAFB
        CMAF_Brands["Compatible Brands:<br/>isom, iso6, cmfc"]
        CMAF_SegExt["Segment Extension:<br/>.cmfv (video) / .cmfa (audio)"]
        CMAF_Profile["DASH Profile:<br/>includes urn:mpeg:dash:<br/>profile:cmaf:2019"]
    end

    subgraph FMP4_Props["fMP4 Properties"]
        style FMP4_Props fill:#14532D,stroke:#22C55E,color:#BBF7D0
        FMP4_Brands["Compatible Brands:<br/>isom, iso6"]
        FMP4_SegExt["Segment Extension:<br/>.m4s"]
        FMP4_Profile["DASH Profile:<br/>urn:mpeg:dash:profile:<br/>isoff-live:2011 only"]
    end

    subgraph Shared["Shared (both formats)"]
        style Shared fill:#374151,stroke:#6B7280,color:#F9FAFB
        MajorBrand["Major Brand: isom"]
        InitExt["Init Extension: .mp4"]
        MIME["MIME: video/mp4 / audio/mp4"]
    end

    subgraph Pipeline["Pipeline Integration"]
        Req["RepackageRequest<br/>.container_format"]
        Init["rewrite_init_segment()<br/>ftyp → build_ftyp(format)"]
        Prog["ProgressiveOutput<br/>segment URIs use<br/>format.video_segment_extension()"]
        Dash["DASH Renderer<br/>MPD @profiles =<br/>format.dash_profiles()"]
    end

    subgraph TS_Props["TS Properties"]
        style TS_Props fill:#5B21B6,stroke:#8B5CF6,color:#F9FAFB
        TS_Brands["Compatible Brands:<br/>N/A (not ISOBMFF)"]
        TS_SegExt["Segment Extension:<br/>.ts"]
        TS_Profile["DASH Profile:<br/>N/A (HLS only)"]
        TS_Enc["Encryption:<br/>AES-128-CBC whole-segment<br/>#EXT-X-KEY:METHOD=AES-128"]
    end

    CMAF --> CMAF_Props
    FMP4 --> FMP4_Props
    TS --> TS_Props

    Req --> Init
    Req --> Prog
    Prog --> Dash

    classDef enum fill:#1F2937,stroke:#F59E0B,color:#F9FAFB
    class CMAF,FMP4,TS enum
```
