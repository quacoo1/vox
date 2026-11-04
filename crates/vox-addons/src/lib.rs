//! # vox-addons
//!
//! Offline voice add-ons (docs/addons-design.md; the wire format is docs/addons-protocol.md). An add-on is a helper
//! program in its own folder that turns text into WAV audio; Vox talks to it with JSON lines over stdin and stdout.
//!
//! | Module | What |
//! |---|---|
//! | [`protocol`] | the message types and the size-capped line reader (pure) |
//! | [`audio`] | checking a helper's WAV and word cues, building the engine's `SynthesisResult` |
//! | [`helper`] | [`HelperClient`]: one contained helper process, its request queue, deadlines, cancel and crash budget |
//! | [`runtime`] | [`AddonRuntime`]: an add-on folder loaded (`addon.json` checked, voices, helper) |
//! | [`host`] | [`AddonHost`]: the `Provider::Addon` speech provider that routes `addon:<id>:<voice>` to its add-on |
//! | [`fake`] | the fake helper behind the `vox-addon-fake` test binary |

pub mod audio;
#[doc(hidden)]
pub mod fake;
pub mod helper;
pub mod host;
pub mod protocol;
pub mod runtime;

pub use helper::{BrokenReason, HelperClient, HelperError, HelperHealth, HelperSpec, HelperTimeouts};
pub use host::{AddonHost, RuntimeStatus};
pub use runtime::{load_spec, AddonRuntime};
