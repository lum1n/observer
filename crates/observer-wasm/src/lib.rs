//! wasm-bindgen surface of observer for browsers and Node, next to
//! acc-wasm's.
//!
//! The module contains acc-wasm, so a page loads one module and gets both:
//! acc's sessions, and harness TUIs the user started themselves. Values are
//! plain JS objects shaped like the core's serde types, as in acc-wasm, and
//! byte-carrying records are acc-wasm's classes ([`AccExec`]). The objects
//! match observer-ffi's under the same names (`tests/parity.rs` keeps the two
//! in step). They throw `Error`s named `AccError` with a `kind`, like
//! acc-wasm's.

use acc_core::Harness;
use acc_core::transcript::Item;
use acc_wasm::AccExec;
use observer::osc7501;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wasm_bindgen::prelude::*;

pub mod observe;

pub use observe::*;

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
export type ProgramStatus =
  | { type: "probe" }
  | { type: "report"; state: RunState | null; id: string | null; app: string | null; title: string | null; message: string | null };
"#;

/// Converts a Rust value to a plain JS value: objects rather than `Map`s,
/// `null` for `None`, numbers for u64 (all values sent fit in 2^53), and
/// `Uint8Array` for bytes.
fn to_js<T: Serialize + ?Sized>(value: &T) -> Result<JsValue, JsError> {
    let serializer = serde_wasm_bindgen::Serializer::new()
        .serialize_maps_as_objects(true)
        .serialize_missing_as_null(true);
    value
        .serialize(&serializer)
        .map_err(|e| JsError::new(&e.to_string()))
}

fn from_js<T: for<'de> Deserialize<'de>>(value: JsValue, what: &str) -> Result<T, JsError> {
    serde_wasm_bindgen::from_value(value).map_err(|e| JsError::new(&format!("invalid {what}: {e}")))
}

/// JS numbers for epoch milliseconds; negative or NaN become 0.
fn ms(now_ms: f64) -> u64 {
    if now_ms.is_finite() && now_ms > 0.0 {
        now_ms as u64
    } else {
        0
    }
}

/// An `Error` named `AccError` whose `kind` says what went wrong.
fn error(kind: &str, message: &str) -> JsValue {
    let e = js_sys::Error::new(message);
    e.set_name("AccError");
    // Setting a property on a fresh object can't fail.
    let _ = js_sys::Reflect::set(&e, &JsValue::from_str("kind"), &JsValue::from_str(kind));
    e.into()
}

/// [`to_js`] for values that always serialize (every core type does).
fn js<T: Serialize + ?Sized>(value: &T) -> JsValue {
    to_js(value).unwrap_or(JsValue::NULL)
}

/// [`from_js`] that throws an `AccError` (`InvalidArgument`).
fn arg<T: for<'de> Deserialize<'de>>(value: JsValue, what: &str) -> Result<T, JsValue> {
    serde_wasm_bindgen::from_value(value)
        .map_err(|e| error("InvalidArgument", &format!("invalid {what}: {e}")))
}

/// An item as the core serializes it, plus its harness-independent `detail`.
fn item_json(i: &Item, h: Harness) -> Value {
    let mut v = serde_json::to_value(i).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "detail".into(),
            serde_json::to_value(i.detail(h)).unwrap_or_default(),
        );
    }
    v
}

fn items<'a>(it: impl Iterator<Item = &'a Item>, h: Harness) -> JsValue {
    js(&it.map(|i| item_json(i, h)).collect::<Vec<Value>>())
}

/// Byte offsets and durations as JS numbers (exact up to 2^53).
fn num(n: u64) -> f64 {
    n as f64
}

fn exec(script: String) -> AccExec {
    acc_core::Exec::new(script).into()
}

/// Follows a harness's own transcript file (Claude, Codex, pi).
#[wasm_bindgen]
#[derive(Debug)]
pub struct TranscriptObserver {
    inner: observer::transcript::Observer,
}

#[wasm_bindgen]
impl TranscriptObserver {
    #[wasm_bindgen(constructor)]
    pub fn new(
        #[wasm_bindgen(unchecked_param_type = "HarnessId")] harness: JsValue,
    ) -> Result<TranscriptObserver, JsError> {
        let h: Harness = from_js(harness, "harness")?;
        let inner = observer::transcript::Observer::new(h).ok_or_else(|| {
            JsError::new(&format!("observer doesn't read {} transcripts", h.display_name()))
        })?;
        Ok(Self { inner })
    }

    /// Feeds bytes appended to the transcript.
    #[wasm_bindgen(unchecked_return_type = "Event[]")]
    pub fn feed(&mut self, bytes: &[u8], now_ms: f64) -> Result<JsValue, JsError> {
        to_js(&self.inner.feed(bytes, ms(now_ms)))
    }
}

/// Bytes a terminal writes back to answer an OSC 7501 probe.
#[wasm_bindgen(js_name = osc7501ProbeReply)]
pub fn osc7501_probe_reply() -> Vec<u8> {
    osc7501::PROBE_REPLY.to_vec()
}

/// Incremental OSC 7501 extractor for a terminal byte stream (e.g. xterm.js output).
#[wasm_bindgen]
#[derive(Debug, Default)]
pub struct Osc7501Scanner {
    inner: osc7501::Scanner,
}

#[wasm_bindgen]
impl Osc7501Scanner {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Messages completed by these bytes.
    #[wasm_bindgen(unchecked_return_type = "ProgramStatus[]")]
    pub fn feed(&mut self, bytes: &[u8]) -> Result<JsValue, JsError> {
        to_js(&self.inner.feed(bytes))
    }
}
