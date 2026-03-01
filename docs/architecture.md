# edgepack Architecture

All diagrams use [Mermaid](https://mermaid.js.org/) syntax. They render natively in Confluence (Mermaid macro), Jira (Mermaid code blocks), GitHub, and can be imported into Lucidchart via **File → Import → Mermaid**.

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
            CacheMod["Cache Client<br/>(AES-256-GCM encrypted)"]
        end
    end

    Origin["Origin Server<br/>CBCS/CENC-encrypted<br/>CMAF content"]
    LicenseServer["DRM License Server<br/>SPEKE 2.0 endpoint"]
    Redis["Redis<br/>(Upstash HTTP or TCP)"]

    Player -- "GET /repackage/{id}/{fmt}/..." --> Cache
    Cache -- "cache miss" --> Handler
    Handler --> Pipeline
    Pipeline --> MediaEngine
    Pipeline --> DRM
    Pipeline --> ManifestGen
    Pipeline --> CacheMod
    CacheMod -- "state, keys,<br/>segments" --> Redis
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
    class Origin,LicenseServer,Redis external
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
        Webhook["webhook.rs<br/>POST webhook +<br/>continue handler"]
    end

    subgraph Repackager["repackager/"]
        RPipeline["pipeline.rs<br/>RepackagePipeline<br/>execute / execute_first /<br/>execute_remaining"]
        Progressive["progressive.rs<br/>ProgressiveOutput<br/>state machine"]
    end

    subgraph Core["Core Modules"]
        Media["media/<br/>ISOBMFF parser<br/>ContainerFormat<br/>codec extraction<br/>init rewrite (ftyp+sinf)<br/>segment rewrite"]
        DRMMod["drm/<br/>EncryptionScheme<br/>SampleDecryptor/Encryptor<br/>SPEKE client + CPIX XML<br/>CBCS decrypt+encrypt<br/>CENC encrypt+decrypt"]
        Manifest["manifest/<br/>HLS renderer<br/>DASH renderer<br/>HLS input parser<br/>DASH input parser"]
        CacheMod2["cache/<br/>CacheBackend trait<br/>EncryptedCacheBackend<br/>Redis HTTP / TCP<br/>In-memory (sandbox)"]
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
    CacheMod2 --> HTTP

    RPipeline --> Config
    RPipeline --> Error
    Router --> Config
```

---

## 4. Split Execution Model (WASI Chaining)

Shows how the pipeline handles WASI request timeouts by splitting work across self-invocations.

```mermaid
sequenceDiagram
    participant Client
    participant CDN as CDN Cache
    participant EP as edgepack
    participant Origin
    participant SPEKE as License Server
    participant Redis

    Note over Client,Redis: Phase 1 — Webhook Trigger (execute_first)
    Client->>EP: POST /webhook/repackage
    EP->>Origin: GET source manifest
    Origin-->>EP: .m3u8 / .mpd
    EP->>Origin: GET init segment
    Origin-->>EP: init.mp4 (CBCS)
    EP->>SPEKE: POST CPIX request
    SPEKE-->>EP: CPIX response (keys + PSSH)
    EP->>Redis: SET keys, rewrite_params, source (encrypted)
    EP->>EP: Rewrite init segment (source→target scheme, target container format)
    EP->>Redis: SET init segment
    EP->>Origin: GET segment_0
    Origin-->>EP: segment_0 (source scheme)
    EP->>EP: Decrypt (source) → Re-encrypt (target)
    EP->>Redis: SET segment_0, manifest_state
    EP-->>Client: 200 OK (manifest_url, segments_completed=1)

    Note over Client,Redis: Phase 2 — Self-Invocation Chain (execute_remaining × N)
    EP->>EP: POST /webhook/repackage/continue
    EP->>Redis: GET rewrite_params, source, manifest_state
    EP->>Origin: GET segment_1
    Origin-->>EP: segment_1 (source scheme)
    EP->>EP: Decrypt → Re-encrypt
    EP->>Redis: SET segment_1, update manifest_state
    EP->>EP: POST /webhook/repackage/continue (next)

    Note over EP,Redis: ... repeats for each segment ...

    EP->>Redis: SET final segment, finalize manifest
    EP->>Redis: DELETE keys, speke, rewrite_params, source
    Note over EP,Redis: Sensitive data cleaned up

    Note over Client,Redis: Phase 3 — Client Playback (concurrent with Phase 2)
    Client->>CDN: GET /repackage/{id}/hls/manifest
    CDN->>Redis: GET manifest_state
    Redis-->>CDN: manifest (live or complete)
    CDN-->>Client: manifest.m3u8 (Cache-Control: max-age=1)
    Client->>CDN: GET /repackage/{id}/hls/init.mp4
    CDN-->>Client: init.mp4 (Cache-Control: immutable, 1yr)
    Client->>CDN: GET /repackage/{id}/hls/segment_0.cmfv
    CDN-->>Client: segment_0.cmfv (Cache-Control: immutable, 1yr)
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
        Sensitive cache keys DELETED.
    end note

    Complete --> [*]
```

---

## 6. Cache Security Model

Shows the two-layer security approach for sensitive data in Redis.

```mermaid
flowchart TB
    subgraph Pipeline["RepackagePipeline"]
        Execute["execute() /<br/>execute_first() /<br/>execute_remaining()"]
        Cleanup["cleanup_sensitive_data()<br/>called on completion"]
    end

    subgraph EncLayer["EncryptedCacheBackend (decorator)"]
        Check{"is_sensitive_key?<br/>(:keys, :speke,<br/>:rewrite_params)"}
        AES["AES-256-GCM<br/>Encrypt/Decrypt"]
        Pass["Pass-through<br/>(no encryption)"]
    end

    subgraph Inner["Inner CacheBackend"]
        RedisHTTP["Redis HTTP<br/>(Upstash)"]
        RedisTCP["Redis TCP"]
        Memory["In-Memory<br/>(sandbox only)"]
    end

    subgraph KeyDerivation["Key Derivation"]
        Token["REDIS_TOKEN<br/>(or sandbox constant)"]
        PRF["AES-128-ECB PRF<br/>2 constant blocks<br/>→ 32-byte key"]
    end

    Execute -- "set(key, value)" --> Check
    Check -- "Yes (sensitive)" --> AES
    Check -- "No" --> Pass
    AES -- "nonce ‖ ciphertext ‖ tag" --> RedisHTTP
    AES -- "nonce ‖ ciphertext ‖ tag" --> RedisTCP
    AES -- "nonce ‖ ciphertext ‖ tag" --> Memory
    Pass -- "plaintext" --> RedisHTTP
    Pass -- "plaintext" --> RedisTCP
    Pass -- "plaintext" --> Memory

    Token --> PRF
    PRF --> AES

    Execute --> Cleanup
    Cleanup -- "DELETE :keys" --> RedisHTTP
    Cleanup -- "DELETE :speke" --> RedisHTTP
    Cleanup -- "DELETE :rewrite_params" --> RedisHTTP
    Cleanup -- "DELETE :source" --> RedisHTTP
```

---

## 7. Cache Key Layout

Shows all Redis keys, their sensitivity classification, TTLs, and lifecycle.

```mermaid
graph LR
    subgraph Sensitive["SENSITIVE (encrypted + deleted on completion)"]
        style Sensitive fill:#7F1D1D,stroke:#EF4444,color:#FCA5A5
        K1["ep:{id}:keys<br/>DRM content keys<br/>TTL: 24h"]
        K2["ep:{id}:speke<br/>SPEKE CPIX response<br/>TTL: 24h"]
        K3["ep:{id}:{fmt}:rewrite_params<br/>encryption keys + IVs<br/>TTL: 48h"]
        K4["ep:{id}:{fmt}:source<br/>source manifest metadata<br/>TTL: 48h"]
    end

    subgraph NonSensitive["NON-SENSITIVE (plaintext, TTL expiry only)"]
        style NonSensitive fill:#14532D,stroke:#22C55E,color:#BBF7D0
        K5["ep:{id}:{fmt}:state<br/>job progress<br/>TTL: 48h"]
        K6["ep:{id}:{fmt}:manifest_state<br/>progressive manifest<br/>TTL: 48h"]
        K7["ep:{id}:{fmt}:init<br/>rewritten init segment<br/>TTL: 48h"]
        K8["ep:{id}:{fmt}:seg:{n}<br/>rewritten media segments<br/>TTL: 48h"]
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

## Key Features Summary

| Feature | Description |
|---------|-------------|
| **Configurable Encryption** | Transforms between CBCS ↔ CENC in any direction; target scheme configurable per request |
| **Configurable Container Format** | Output as CMAF (`.cmfv`, `cmfc` brand) or fMP4 (`.m4s`); ftyp rewriting, dynamic DASH profiles |
| **Source Scheme Auto-Detection** | Detects source encryption from init segment `schm` box or manifest DRM signaling |
| **Trait-Based Crypto Dispatch** | `SampleDecryptor`/`SampleEncryptor` traits with factory functions for scheme-agnostic pipeline |
| **Progressive Output** | Clients can begin playback as soon as the first segment is ready |
| **Split Execution** | WASI-compatible self-invocation chaining avoids request timeouts |
| **Encryption at Rest** | Sensitive cache entries (DRM keys, SPEKE responses) encrypted with AES-256-GCM |
| **Immediate Cleanup** | All sensitive data deleted from cache the moment processing completes |
| **Aggressive CDN Caching** | Segments and finalized manifests cached for 1 year; live manifests refresh every second |
| **Multi-DRM** | Widevine + PlayReady for CENC output; FairPlay + Widevine + PlayReady for CBCS output |
| **Multi-Key DRM** | Per-track keying (separate video/audio KIDs), multi-KID PSSH v1 boxes, TrackKeyMapping |
| **Codec Awareness** | RFC 6381 codec string extraction from init segments for manifest signaling |
| **Zero External Test Dependencies** | All 709 tests use synthetic CMAF fixtures — no network or media files needed |
| **WASM-Native** | Entire runtime compiles to `wasm32-wasip2` with no async runtime or system calls |

## Inputs and Outputs

| Direction | What | Format | Protocol |
|-----------|------|--------|----------|
| **Input** | Source manifest | HLS `.m3u8` or DASH `.mpd` (source scheme auto-detected) | HTTP GET from origin |
| **Input** | Source init segment | CMAF (CBCS or CENC sinf/schm/tenc/pssh) | HTTP GET from origin |
| **Input** | Source media segments | CMAF (source-scheme encrypted mdat) | HTTP GET from origin |
| **Input** | DRM content keys | CPIX XML (SPEKE 2.0) | HTTP POST to license server |
| **Output** | Repackaged manifest | HLS `.m3u8` or DASH `.mpd` (target-scheme DRM signaling, format-aware profiles) | HTTP GET via CDN |
| **Output** | Repackaged init segment | CMAF or fMP4 (target-scheme schm/tenc/pssh, target-format ftyp brands, DRM systems per scheme) | HTTP GET via CDN |
| **Output** | Repackaged media segments | CMAF `.cmfv` or fMP4 `.m4s` (target-scheme encrypted mdat) | HTTP GET via CDN |
| **Output** | Job status | JSON | HTTP GET via CDN |

---

## Completed Architecture Extensions

### ~~Phase 2: Container Format Flexibility~~ ✅ Complete
- `ContainerFormat` enum (`Cmaf`, `Fmp4`, `Iso`) in `src/media/container.rs` with brand, extension, profile helpers
- ftyp box rewriting in init segments for output container format
- Dynamic segment extensions (`.cmfv`/`.cmfa` for CMAF, `.m4s` for fMP4, `.mp4` for ISO)
- Dynamic DASH profile signaling (`cmaf:2019` for CMAF, `isoff-live:2011` for fMP4/ISO)
- `container_format` threaded through `RepackageRequest` → `ContinuationParams` → `ManifestState` → `ProgressiveOutput`
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
- Per-scheme `ContinuationParams`, init segments, manifest state in Redis

### ~~Phase 5: Multi-Key DRM & Codec Awareness~~ ✅ Complete
- `TrackKeyMapping` type mapping `TrackType → [u8; 16]` KID for per-track keying
- Per-track `tenc` in init segments — video and audio tracks get different KIDs via `hdlr` detection
- Multi-KID PSSH v1 — grouped by `system_id`, all track KIDs embedded per DRM system
- Codec string extraction via `extract_tracks()` in `src/media/codec.rs` — RFC 6381 codec strings from stsd config boxes (avcC, hvcC, esds, vpcC, av1C)
- Timescale parsing from `mdhd` box
- Pipeline integration: `extract_tracks()` → `build_track_key_mapping()` → multi-KID SPEKE → per-track init rewriting
- Codec strings populated into `VariantInfo` for HLS `CODECS=` and DASH `codecs=` manifest attributes
- `TrackKeyMapping` serialized via `ContinuationParams` for split execution

## Planned Architecture Extensions

### Phase 6: Subtitle & Text Track Pass-Through — P0
- WebVTT (`wvtt`) and TTML (`stpp`) sample entry pass-through in fMP4
- CEA-608/708 manifest signaling
- HLS subtitle rendition groups, DASH subtitle AdaptationSets

### Phase 8: JIT Packaging (On-Demand GET) — P0
- Manifest-on-GET, Init-on-GET, Segment-on-GET (lazy repackaging)
- Request coalescing via Redis locking
- Hybrid mode (JIT + proactive webhook coexist)

### Phase 17: CDN Provider Adapters & Binary Optimization — P0
- Cloudflare Workers, Fastly Compute, AWS Lambda@Edge adapters
- WASI Preview 1 fallback shim
- Binary profiling with `twiggy` + `wasm-opt`

---

## 10. Container Format Comparison

Shows the differences between CMAF and fMP4 output formats and how they flow through the system.

```mermaid
graph TB
    subgraph ContainerFormat["ContainerFormat Enum"]
        CMAF["CMAF<br/>(Common Media Application Format)"]
        FMP4["fMP4<br/>(Fragmented MP4)"]
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
        Cont["ContinuationParams<br/>.container_format<br/>(serialized to Redis)"]
    end

    CMAF --> CMAF_Props
    FMP4 --> FMP4_Props

    Req --> Init
    Req --> Cont
    Cont --> Prog
    Prog --> Dash

    classDef enum fill:#1F2937,stroke:#F59E0B,color:#F9FAFB
    class CMAF,FMP4 enum
```
