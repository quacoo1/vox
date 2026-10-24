//! # vox-tts
//!
//! Speech synthesis (tts.md). Depends on vox-text (chunking, context windows, sentence starts) — the one lib→lib
//! dependency besides vox-core, because the engine chunks the text it synthesizes.
//!
//! | Module | Spec |
//! |---|---|
//! | [`error`] | tts.md §2.4 (error model, classification, user-facing messages from `vox_core::read::eleven_messages`) |
//! | [`mapping`] | tts.md §2.5 (tolerant response mapping, voice sort, lenient base64) |
//! | [`client`] | tts.md §2.3, §2.6–§2.11 (transport, synthesis + parameter fallback, voices/models/account, memo caches, key validation) |
//! | [`engine`] | tts.md §4 (sessions, sequential synthesis, retries, bounded look-ahead, stop/supersede, cached playback) |
//! | [`cancel`] | the cancellation token used by the engine and the client |
//! | [`mock`] | tts.md §5 (offline synthesiser) |
//! | [`wav`] | tts.md §6 |
//! | [`duration`] | tts.md §4.8 (`chunkDuration`) |
//! | [`cached`] | reader-playback.md §2.7 (`previewReplay`, `clipAlignment`, `withoutText`) |
//!
//! Threading: blocking `ureq` on worker threads. One engine session = one worker thread doing sequential requests;
//! cancellation is cooperative (a cancelled in-flight request runs to completion or timeout; its audio is billed but
//! never delivered; every wait point re-checks liveness). Network policy (system proxy, Windows certificate store,
//! user agent, no timeout retries, remembered parameter rejections): [`client`] and [`error`] module docs.
//! Owner: the vox-tts engineer.

// `TtsError` (the fixed cross-crate error type with its public fields, ARCHITECTURE.md Appendix A.4) is ~144 bytes and
// returned by value from the public API by design; errors are rare and never on a hot path, so boxing would only
// change the contract.
#![allow(clippy::result_large_err)]

pub mod cached;
pub mod cancel;
pub mod client;
pub mod duration;
pub mod engine;
pub mod error;
mod jsval;
pub mod mapping;
pub mod mock;
pub mod wav;

pub use cancel::CancelToken;
pub use client::{user_agent, ClientConfig, ElevenLabsClient, KeySource, ProxyConfig, SynthesisRequest, SynthesisResult};
pub use engine::{EngineEvent, PlayCachedOptions, StartSessionOptions, TtsEngine, TtsSink};
pub use error::TtsError;
