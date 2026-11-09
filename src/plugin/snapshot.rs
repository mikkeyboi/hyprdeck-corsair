//! Cheap per-keyboard readback for `state` (DESIGN §6 "State cost budget"): only GETs, plus an
//! effect readback (software-mode dip) only when nothing is cached; refresh never switches mode.

use super::backend::{AnyTransport, Backend, Session, session};
use crate::bragi::{
    self, Bragi, PROP_APP_FW, PROP_BATTERY, PROP_BOOT_FW, PROP_BRIGHTNESS, PROP_BRIGHTNESS_2,
    PROP_CHARGE, PROP_LAYOUT, PROP_LOCK_ENABLE, PROP_LOCK_MASK, PROP_MAX_POLLING, PROP_MODE,
    PROP_POLLING, PROP_RADIO_FW, RES_K65_EFFECT,
};
use crate::devices::{Keyboard, LinkKind, Model};
use crate::effects::{self, Decoded};
use crate::ipc::{self, DeviceStatus};
use crate::storage::ValueCache;
use anyhow::{Context, Result};
use serde_json::json;
use std::collections::BTreeMap;

/// Effect readback cadence during plain `state` refreshes.
pub const EFFECT_MAX_AGE: u64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    Online,
    /// Receiver reports the keyboard off, or the keyboard answered status 6.
    Asleep,
    /// Could not talk to the keyboard at all (permission, busy, timeout…).
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct EffectReading {
    /// Stored descriptor bytes (as read back or as last written by hyprdeck).
    pub raw: Vec<u8>,
    pub read_at: u64,
    /// Set when the latest readback attempt failed (the cached bytes may still be shown).
    pub error: Option<String>,
}

impl EffectReading {
    pub fn decoded(&self) -> Option<Decoded> {
        (!self.raw.is_empty()).then(|| effects::decode(&self.raw))
    }
    pub fn effect(&self) -> Option<effects::Effect> {
        match self.decoded()? {
            Decoded::Known(effect) => Some(effect),
            Decoded::Unknown { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub link: Link,
    /// The companion owns the keyboard; requests went through its socket.
    pub via_daemon: bool,
    /// Raw value bytes (reply[3..7]) of every successful GET.
    pub props: BTreeMap<u8, Vec<u8>>,
    pub prop_errors: BTreeMap<u8, String>,
    pub receiver_firmware: Option<Vec<u8>>,
    pub effect: Option<EffectReading>,
}

impl Snapshot {
    pub fn new(link: Link, via_daemon: bool) -> Self {
        Self {
            link,
            via_daemon,
            props: BTreeMap::new(),
            prop_errors: BTreeMap::new(),
            receiver_firmware: None,
            effect: None,
        }
    }
    pub fn value(&self, prop: u8) -> Option<u32> {
        let bytes = self.props.get(&prop)?;
        let mut le = [0u8; 4];
        for (slot, byte) in le.iter_mut().zip(bytes) {
            *slot = *byte;
        }
        Some(u32::from_le_bytes(le))
    }
    pub fn online(&self) -> bool {
        self.link == Link::Online
    }
}

/// GETs done on every refresh (≈ 12 packets).
pub fn state_props(keyboard: &Keyboard) -> Vec<u8> {
    let caps = keyboard.model.capabilities(keyboard.link);
    let mut props = vec![PROP_MODE, PROP_BRIGHTNESS];
    if caps.battery {
        props.extend([PROP_BATTERY, PROP_CHARGE]);
    }
    props.extend([PROP_APP_FW, PROP_BOOT_FW]);
    if keyboard.model == Model::K65PlusWireless {
        props.push(PROP_RADIO_FW);
    }
    props.extend([
        PROP_LAYOUT,
        PROP_LOCK_ENABLE,
        PROP_LOCK_MASK,
        PROP_POLLING,
        PROP_MAX_POLLING,
    ]);
    if caps.secondary_brightness {
        props.push(PROP_BRIGHTNESS_2);
    }
    props
}

/// Whether the daemon owns `keyboard` (its requests must go through the socket).
pub fn owned_by_daemon(daemon: Option<&[DeviceStatus]>, keyboard: &Keyboard) -> bool {
    daemon
        .into_iter()
        .flatten()
        .any(|status| status.id == keyboard.id && status.owned)
}

/// Read one keyboard. Never fails: problems become `Link::Failed` / per-prop errors.
pub fn read(backend: &dyn Backend, keyboard: &Keyboard, via_daemon: bool, now: u64) -> Snapshot {
    let mut snapshot = Snapshot::new(Link::Online, via_daemon);
    if keyboard.link == LinkKind::Receiver && !via_daemon {
        match read_receiver(backend, keyboard) {
            Ok((online, firmware)) => {
                snapshot.receiver_firmware = firmware;
                if !online {
                    snapshot.link = Link::Asleep;
                    snapshot.effect = cached_effect(&keyboard.id);
                    return snapshot;
                }
            }
            Err(error) => {
                snapshot.link = Link::Failed(format!("{error:#}"));
                return snapshot;
            }
        }
    }
    let mut bragi = match session(backend, keyboard, via_daemon) {
        Ok(bragi) => bragi,
        Err(error) => {
            snapshot.link = Link::Failed(format!("{error:#}"));
            return snapshot;
        }
    };
    for prop in state_props(keyboard) {
        match bragi.get(prop) {
            Ok(reply) => {
                let data = reply.data();
                snapshot
                    .props
                    .insert(prop, data[..data.len().min(4)].to_vec());
            }
            Err(error) if bragi::is_offline(&error) => {
                snapshot.link = Link::Asleep;
                snapshot.effect = cached_effect(&keyboard.id);
                return snapshot;
            }
            Err(error) if bragi::is_timeout(&error) => {
                snapshot.link = Link::Failed(format!(
                    "The keyboard stopped answering ({error:#}) — check the cable or receiver"
                ));
                return snapshot;
            }
            Err(error) => {
                snapshot.prop_errors.insert(prop, format!("{error:#}"));
            }
        }
    }
    let caps = keyboard.model.capabilities(keyboard.link);
    if caps.onboard_effects {
        snapshot.effect = effect_for_state(&mut bragi, &keyboard.id, now);
        if snapshot
            .effect
            .as_ref()
            .is_some_and(|reading| reading.error.is_some())
        {
            // A failed mode guard cannot leave the pre-dip mode snapshot authoritative.
            let restoration_failed = snapshot
                .effect
                .as_ref()
                .and_then(|reading| reading.error.as_deref())
                .is_some_and(|error| error.contains("restore") || error.contains("Rescue"));
            match bragi.mode() {
                Ok(_) if restoration_failed => {
                    snapshot.props.remove(&PROP_MODE);
                    snapshot.prop_errors.insert(
                        PROP_MODE,
                        "Mode restoration failed — Rescue required".into(),
                    );
                }
                Ok(mode) => {
                    snapshot.props.insert(PROP_MODE, vec![mode]);
                }
                Err(error) => {
                    snapshot.props.remove(&PROP_MODE);
                    snapshot.prop_errors.insert(
                        PROP_MODE,
                        format!("Mode uncertain after effect read: {error:#}"),
                    );
                }
            }
        }
    }
    snapshot
}

fn read_receiver(backend: &dyn Backend, keyboard: &Keyboard) -> Result<(bool, Option<Vec<u8>>)> {
    let transport = backend.open_receiver(keyboard)?;
    let mut receiver = Bragi::new(AnyTransport(transport), keyboard.model);
    let online = bragi::receiver_children_online(&mut receiver)
        .context("Asking the receiver whether the keyboard is on")?;
    let firmware = receiver.get(PROP_APP_FW).ok().map(|reply| {
        let data = reply.data();
        data[..data.len().min(4)].to_vec()
    });
    Ok((online, firmware))
}

/// Cached effect bytes and read time.
pub fn cached_effect(device: &str) -> Option<EffectReading> {
    let raw = ValueCache::get(device, "effect_raw")?;
    let raw = ipc::from_hex(raw.as_str()?).ok()?;
    let read_at = ValueCache::get(device, "effect_read_at")
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    Some(EffectReading {
        raw,
        read_at,
        error: None,
    })
}

/// Remember effect bytes (read back or just written) with their time.
pub fn cache_effect(device: &str, raw: &[u8], now: u64) {
    let stored = ValueCache::set(device, "effect_raw", json!(ipc::to_hex(raw)))
        .and_then(|()| ValueCache::set(device, "effect_read_at", json!(now)));
    if let Err(error) = stored {
        eprintln!("hyprdeck-corsair: caching the effect failed: {error:#}");
    }
}

/// Read the live effect slot inside one software-mode dip and cache it.
pub fn read_effect(bragi: &mut Session, device: &str, now: u64) -> Result<EffectReading> {
    let raw = bragi
        .with_software_mode(|bragi| bragi.read_resource(RES_K65_EFFECT))
        .context("Reading the current effect")?;
    cache_effect(device, &raw, now);
    Ok(EffectReading {
        raw,
        read_at: now,
        error: None,
    })
}

fn effect_for_state(bragi: &mut Session, device: &str, now: u64) -> Option<EffectReading> {
    let cached = cached_effect(device);
    // A plain refresh never switches the keyboard's mode: any cached value is used as is.
    // The slot is read from the keyboard only when nothing is cached yet, after a lighting
    // action, or on the explicit "Re-read from keyboard" action.
    if cached.is_some() {
        return cached;
    }
    let attempted = ValueCache::get(device, "effect_attempt_at").and_then(|value| value.as_u64());
    if attempted.is_some_and(|at| at <= now && now.saturating_sub(at) < EFFECT_MAX_AGE) {
        let mut reading = cached.unwrap_or(EffectReading {
            raw: Vec::new(),
            read_at: 0,
            error: None,
        });
        reading.error =
            Some("Recent effect read failed; showing last-known value until retry".into());
        return Some(reading);
    }
    // Record even unsuccessful attempts so automatic refresh cannot repeatedly toggle mode.
    if let Err(error) = ValueCache::set(device, "effect_attempt_at", json!(now)) {
        return Some(EffectReading {
            raw: Vec::new(),
            read_at: 0,
            error: Some(format!("Cannot persist readback cadence: {error:#}")),
        });
    }
    match read_effect(bragi, device, now) {
        Ok(reading) => Some(reading),
        Err(error) => Some(match cached {
            Some(mut reading) => {
                reading.error = Some(format!("{error:#}"));
                reading
            }
            None => EffectReading {
                raw: Vec::new(),
                read_at: 0,
                error: Some(format!("{error:#}")),
            },
        }),
    }
}
