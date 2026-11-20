//! Pending pairing requests: two-minute lifetime, X25519 + HKDF key derivation
//! and six-digit code verification.

use std::collections::HashMap;
use std::sync::Mutex;

use barge_machine::{
    auth, parse_public_key, KeyPair, PairResponse, PairRole, PairStatus, PairStatusResponse,
    PairedClient, PendingPairRequest,
};

use crate::state::DaemonState;

/// How long a pairing request stays pending before expiring.
pub const PAIR_TTL_MS: u64 = 120_000;
const RETAIN_DECIDED_MS: u64 = 300_000;
const MAX_PENDING: usize = 16;

#[derive(Debug, Clone)]
enum Decision {
    Pending {
        key: [u8; 32],
    },
    Approved {
        client_id: String,
        decided_at_ms: u64,
    },
    Denied {
        decided_at_ms: u64,
    },
    Expired {
        expired_at_ms: u64,
    },
}

#[derive(Debug, Clone)]
struct Entry {
    request_id: String,
    client_name: String,
    code: String,
    created_at_ms: u64,
    expires_at_ms: u64,
    decision: Decision,
}

#[derive(Debug)]
pub enum PairStartError {
    InvalidInput(String),
    TooManyPending,
    Internal(String),
}

#[derive(Debug)]
pub enum PairDecisionError {
    NotFound,
    AlreadyDecided(PairStatus),
    Expired,
    Internal(String),
}

#[derive(Debug, Default)]
pub struct PairingStore {
    entries: Mutex<HashMap<String, Entry>>,
}

impl PairingStore {
    pub fn start(
        &self,
        client_name: &str,
        client_public_hex: &str,
        now_ms: u64,
    ) -> std::result::Result<(PairResponse, String), PairStartError> {
        let client_name = client_name.trim();
        if client_name.is_empty()
            || client_name.len() > 64
            || client_name.chars().any(char::is_control)
        {
            return Err(PairStartError::InvalidInput(
                "client_name must be 1-64 printable characters".to_string(),
            ));
        }
        let client_public = parse_public_key(client_public_hex)
            .map_err(|e| PairStartError::InvalidInput(e.message().to_string()))?;
        let daemon_keys =
            KeyPair::generate().map_err(|e| PairStartError::Internal(e.message().to_string()))?;
        let derived = daemon_keys
            .derive(&client_public, PairRole::Daemon)
            .map_err(|e| PairStartError::InvalidInput(e.message().to_string()))?;
        let request_id =
            auth::hex(&auth::random(16).map_err(|e| {
                PairStartError::Internal(format!("cannot generate request ID: {e}"))
            })?);
        let expires_at_ms = now_ms.saturating_add(PAIR_TTL_MS);

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune(&mut entries, now_ms);
        let active = entries
            .values()
            .filter(|entry| matches!(entry.decision, Decision::Pending { .. }))
            .count();
        if active >= MAX_PENDING {
            return Err(PairStartError::TooManyPending);
        }

        let code = derived.code().to_string();
        let entry = Entry {
            request_id: request_id.clone(),
            client_name: client_name.to_string(),
            code: code.clone(),
            created_at_ms: now_ms,
            expires_at_ms,
            decision: Decision::Pending {
                key: *derived.key(),
            },
        };
        entries.insert(request_id.clone(), entry);

        Ok((
            PairResponse::new(request_id, daemon_keys.public_key_hex(), expires_at_ms),
            code,
        ))
    }

    pub fn status(&self, request_id: &str, now_ms: u64) -> Option<PairStatusResponse> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune(&mut entries, now_ms);
        let entry = entries.get_mut(request_id)?;
        advance_expiry(entry, now_ms);
        let (status, client_id) = match &entry.decision {
            Decision::Pending { .. } => (PairStatus::Pending, None),
            Decision::Approved { client_id, .. } => (PairStatus::Approved, Some(client_id.clone())),
            Decision::Denied { .. } => (PairStatus::Denied, None),
            Decision::Expired { .. } => (PairStatus::Expired, None),
        };
        Some(PairStatusResponse::new(
            entry.request_id.clone(),
            status,
            client_id,
        ))
    }

    pub fn pending(&self, now_ms: u64) -> Vec<PendingPairRequest> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune(&mut entries, now_ms);
        let mut list: Vec<PendingPairRequest> = entries
            .values()
            .filter(|entry| matches!(entry.decision, Decision::Pending { .. }))
            .map(|entry| PendingPairRequest {
                request_id: entry.request_id.clone(),
                client_name: entry.client_name.clone(),
                code: entry.code.clone(),
                created_at_unix_ms: entry.created_at_ms,
                expires_at_unix_ms: entry.expires_at_ms,
            })
            .collect();
        list.sort_by_key(|item| item.created_at_unix_ms);
        list
    }

    pub fn approve(
        &self,
        request_id: &str,
        state: &DaemonState,
        now_ms: u64,
    ) -> std::result::Result<PairedClient, PairDecisionError> {
        let (client_name, key) = {
            let mut entries = self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            prune(&mut entries, now_ms);
            let entry = entries
                .get_mut(request_id)
                .ok_or(PairDecisionError::NotFound)?;
            advance_expiry(entry, now_ms);
            match &entry.decision {
                Decision::Pending { key } => (entry.client_name.clone(), *key),
                Decision::Approved { .. } => {
                    return Err(PairDecisionError::AlreadyDecided(PairStatus::Approved))
                }
                Decision::Denied { .. } => {
                    return Err(PairDecisionError::AlreadyDecided(PairStatus::Denied))
                }
                Decision::Expired { .. } => return Err(PairDecisionError::Expired),
            }
        };

        let record = state
            .add_client(&client_name, &key, now_ms)
            .map_err(|e| PairDecisionError::Internal(e.message().to_string()))?;

        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = entries.get_mut(request_id) {
            entry.decision = Decision::Approved {
                client_id: record.id.clone(),
                decided_at_ms: now_ms,
            };
        }
        Ok(record.summary())
    }

    pub fn deny(
        &self,
        request_id: &str,
        now_ms: u64,
    ) -> std::result::Result<(), PairDecisionError> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune(&mut entries, now_ms);
        let entry = entries
            .get_mut(request_id)
            .ok_or(PairDecisionError::NotFound)?;
        advance_expiry(entry, now_ms);
        match &entry.decision {
            Decision::Pending { .. } => {
                entry.decision = Decision::Denied {
                    decided_at_ms: now_ms,
                };
                Ok(())
            }
            Decision::Approved { .. } => {
                Err(PairDecisionError::AlreadyDecided(PairStatus::Approved))
            }
            Decision::Denied { .. } => Err(PairDecisionError::AlreadyDecided(PairStatus::Denied)),
            Decision::Expired { .. } => Err(PairDecisionError::Expired),
        }
    }
}

fn advance_expiry(entry: &mut Entry, now_ms: u64) {
    if matches!(entry.decision, Decision::Pending { .. }) && now_ms >= entry.expires_at_ms {
        entry.decision = Decision::Expired {
            expired_at_ms: entry.expires_at_ms,
        };
    }
}

fn prune(entries: &mut HashMap<String, Entry>, now_ms: u64) {
    for entry in entries.values_mut() {
        advance_expiry(entry, now_ms);
    }
    entries.retain(|_, entry| match entry.decision {
        Decision::Pending { .. } => true,
        Decision::Approved { decided_at_ms, .. } | Decision::Denied { decided_at_ms } => {
            now_ms.saturating_sub(decided_at_ms) < RETAIN_DECIDED_MS
        }
        Decision::Expired { expired_at_ms } => {
            now_ms.saturating_sub(expired_at_ms) < RETAIN_DECIDED_MS
        }
    });
}
