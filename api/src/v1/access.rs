//! Keys limited to some agents (`api_keys.agents`). A session belongs to the
//! agent in its `metadata.iron_fleet_agent`, which the control plane stamps
//! on every session it creates and which never changes — so the lookup is
//! cached for the life of the process. Unlimited keys never look anything up.
//!
//! A session the key may not reach is a 404, as if it did not exist: a
//! limited key learns nothing about the rest of the fleet.

use super::AppState;
use crate::auth::middleware::Principal;
use crate::error::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Past this many entries the cache starts over; a lookup is one cheap call.
const MAX_CACHED: usize = 4096;

/// Session id → agent slug.
#[derive(Clone, Default)]
pub struct SessionAgents {
    inner: Arc<Mutex<HashMap<String, String>>>,
}

impl SessionAgents {
    fn get(&self, id: &str) -> Option<String> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    fn put(&self, id: &str, agent: &str) {
        let mut map = self.inner.lock().unwrap();
        if map.len() >= MAX_CACHED {
            map.clear();
        }
        map.insert(id.to_owned(), agent.to_owned());
    }
}

/// The agent a session object belongs to, if it says.
pub fn session_agent(session: &Value) -> Option<&str> {
    session["metadata"]["iron_fleet_agent"].as_str()
}

/// Note what a session object says about its agent (for a limited key's
/// next call), and check the key may see it.
pub fn check_object(state: &AppState, who: &Principal, id: &str, session: &Value) -> Result<()> {
    if who.agents.is_none() {
        return Ok(());
    }
    let agent = session_agent(session).unwrap_or_default();
    if !agent.is_empty() {
        state.session_agents.put(id, agent);
    }
    if who.may_reach(agent) {
        Ok(())
    } else {
        Err(Error::NotFound)
    }
}

/// Before anything is done with session `id`: a limited key must be able to
/// reach its agent.
pub async fn check_session(state: &AppState, who: &Principal, id: &str) -> Result<()> {
    if who.agents.is_none() {
        return Ok(());
    }
    if let Some(agent) = state.session_agents.get(id) {
        return if who.may_reach(&agent) {
            Ok(())
        } else {
            Err(Error::NotFound)
        };
    }
    let session = state
        .control_plane
        .get(&format!("/sessions/{id}"), &[])
        .await?;
    check_object(state, who, id, &session)
}

/// Keep the list rows (`{data: [...]}` or a bare array) whose `field` names
/// an agent this key may reach.
pub fn retain_reachable(who: &Principal, rows: &mut Value, field: impl Fn(&Value) -> Option<&str>) {
    if who.agents.is_none() {
        return;
    }
    let list = match rows {
        Value::Array(list) => Some(list),
        Value::Object(map) => map.get_mut("data").and_then(Value::as_array_mut),
        _ => None,
    };
    if let Some(list) = list {
        list.retain(|row| field(row).is_some_and(|a| who.may_reach(a)));
    }
}
