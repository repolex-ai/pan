//! Detector calls — external, configured endpoints. Pan ships ZERO models.
//!
//! A detector is a `{role → endpoint URL}` entry in pan.yml. Every role is
//! optional; "endpoint not configured = stage skipped" (Pool's proven
//! best-effort pattern, minus the queue it was welded to). The heavy,
//! GPU-bound, opinionated part lives behind a URL, not inside this binary.
//!
//! v1 wires the `embed` role (it feeds the crown jewel). The endpoint
//! contract is deliberately tiny:
//!
//!   POST <endpoint>
//!     body:   the raw media bytes
//!     header: Content-Type: <media type>
//!   → 200 JSON: {"vector": [f32, …]}   (1-D, L2-normalized)
//!
//! Anything (Iris, a local ollama shim, someone else's service) that speaks
//! that shape plugs in. Other roles (caption/pose/sam3) are config-shape-ready
//! and land as they're built.

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;

pub const ROLE_EMBED: &str = "embed";

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    vector: Vec<f32>,
}

#[derive(Clone)]
pub struct Detectors {
    endpoints: HashMap<String, String>,
    client: reqwest::Client,
}

impl Detectors {
    pub fn new(endpoints: HashMap<String, String>) -> Self {
        Self {
            endpoints,
            client: reqwest::Client::new(),
        }
    }

    pub fn is_configured(&self, role: &str) -> bool {
        self.endpoints.contains_key(role)
    }

    /// Call the embed detector. `Ok(None)` = not configured (stage skipped —
    /// not an error). `Err` = configured but the call failed (the caller
    /// decides whether that's fatal; storing media never is).
    pub async fn embed(&self, bytes: &[u8], media_type: &str) -> Result<Option<Vec<f32>>> {
        let url = match self.endpoints.get(ROLE_EMBED) {
            Some(u) => u,
            None => return Ok(None),
        };
        let resp = self
            .client
            .post(url)
            .header("Content-Type", media_type)
            .body(bytes.to_vec())
            .send()
            .await
            .with_context(|| format!("embed detector unreachable at {url}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("embed detector {url} returned {}", resp.status()));
        }
        let parsed: EmbedResponse = resp
            .json()
            .await
            .context("embed detector response is not {\"vector\": [f32…]}")?;
        if parsed.vector.is_empty() {
            return Err(anyhow!("embed detector returned an empty vector"));
        }
        Ok(Some(parsed.vector))
    }
}
