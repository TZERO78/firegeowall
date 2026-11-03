//! `<state_dir>/state.json`: when lists were fetched, from where, how big,
//! running drop totals across rebuilds, and the last failure if any.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::nft::Live;
use crate::util;
use crate::zones::{Family, Zone};

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub last_update: Option<u64>,
    pub last_apply: Option<u64>,
    /// Set when the last `fgw update` failed; cleared by the next good one.
    pub degraded: Option<String>,
    pub countries: BTreeMap<String, CountryState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CountryState {
    pub source: String,
    pub fetched: u64,
    pub v4: usize,
    pub v6: usize,
    /// Drops counted before the last rebuild (nft counters restart at 0).
    pub dropped_v4: u64,
    pub dropped_v6: u64,
}

fn path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

impl State {
    pub fn load(dir: &Path) -> State {
        std::fs::read(path(dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let mut b = serde_json::to_vec_pretty(self)?;
        b.push(b'\n');
        util::write_atomic(&path(dir), &b, 0o644).context("cannot save state")
    }

    pub fn record_fetch(&mut self, cc: &str, source: &str, z: &Zone) {
        let e = self.countries.entry(cc.to_string()).or_default();
        e.source = source.to_string();
        e.fetched = util::now();
        e.v4 = z.v4.len();
        e.v6 = z.v6.len();
    }

    /// Fold the live counters into the running totals right before a rebuild
    /// resets them.
    pub fn bank_counters(&mut self, live: &Live) {
        for (cc, e) in self.countries.iter_mut() {
            e.dropped_v4 += live.dropped(cc, Family::V4);
            e.dropped_v6 += live.dropped(cc, Family::V6);
        }
    }
}
