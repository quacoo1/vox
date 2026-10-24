//! # vox-audio
//!
//! Playback of TTS sessions (reader-playback.md §4–§6, §9 position state). In the Electron app the widget's
//! `<audio>` elements did this; here the core owns one [`Player`] that runs whether or not the widget is visible.
//!
//! | Module | Role / spec |
//! |---|---|
//! | [`player`] | **Cross-crate API**: `Player` handle, `PlayerEvent`, `PlayerConfig` (reader-playback.md §4.3–§4.10) |
//! | [`timeline`] | pure timeline maths: `dur`, available, position, duration estimate, char index, time at char (§4.8) |
//! | [`clock`] | the listening clock (`listenedSec`, §4.9), pure with an injected clock |
//! | [`decode`] | symphonia MP3 (ElevenLabs) and WAV (mock) → f32 PCM |
//! | [`stretch`] | WSOLA time-stretch 0.5×–3× without pitch change (speech quality) |
//! | [`resample`] | source rate → device rate (polyphase windowed sinc) |
//! | [`output`] | WASAPI output via cpal: lock-free block queue, volume, flush-by-generation, rendered-frames clock |
//! | [`warmer`] | the output warmer state machine (§6) |
//!
//! Private: `engine` (the player state machine, driven by the player thread), `feeder` (decoded chunks → stretched,
//! resampled, tagged blocks; gapless joins), `shared` (the state behind [`Player::snapshot`]).
//!
//! Only `player` is used by other crates; the internal module APIs below are the owner's starting point and may be
//! refined as long as `player`'s surface stays as documented. Owner: the vox-audio engineer.

pub mod clock;
pub mod decode;
mod engine;
mod feeder;
pub mod output;
pub mod player;
pub mod resample;
mod shared;
pub mod stretch;
pub mod timeline;
pub mod warmer;

pub use player::{Player, PlayerConfig, PlayerEvent};
