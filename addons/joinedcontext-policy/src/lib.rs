// SPDX-License-Identifier: EUPL-1.2
//! The joinedcontext platform's policy engine (ADR-0020 addon, README):
//! the Tenant's Policy rules, loaded through `/q/policy/{tenant}`, decided
//! on the `Operation` and the `Subject` alone. No network, no store, no
//! token: the gateway verified the caller and forwards who it is.
//!
//! ```
//! use antares_api::policy::PolicyEngine;
//! use joinedcontext_policy::JoinedcontextPolicy;
//! use serde_json::json;
//!
//! let engine = JoinedcontextPolicy::new();
//! engine.load("city", &json!({"policies": [{
//!     "id": "open-data", "effect": "permit",
//!     "assignee": {"kind": "role", "id": "public"},
//!     "operations": ["5.7.1", "5.7.2"],
//!     "information": [{"entities": [{"type": "Parking"}]}]
//! }]}))?;
//! assert!(engine.table("city").is_some());
//! assert!(engine.load("city", &json!({"policies": [{"id": ""}]})).is_err());
//! assert!(engine.table("city").is_some(), "a refused document keeps the last one");
//! # Ok::<(), String>(())
//! ```

pub mod access;
pub mod decide;
pub mod document;
pub mod names;
pub mod odrl;
pub mod subject;
pub mod surface;
pub mod ucast;

use antares_api::policy::{
    Decision, DecisionFuture, NotifyDecision, Operation, PolicyEngine, Subject,
};
use document::Table;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};
use subject::Caller;

/// The name the engine is selected by (`ANTARES_POLICY=joinedcontext`).
pub const POLICY_NAME: &str = "joinedcontext";

/// The engine: each Tenant's compiled document, swapped whole on load.
#[derive(Default)]
pub struct JoinedcontextPolicy {
    tables: RwLock<HashMap<String, Arc<Table>>>,
}

impl JoinedcontextPolicy {
    /// An engine with no document: it denies every platform caller.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The engine of this process: the one the broker decides with and the
    /// access surface reads, so the two answer from one table.
    #[must_use]
    pub fn process() -> Arc<Self> {
        static ENGINE: std::sync::OnceLock<Arc<JoinedcontextPolicy>> = std::sync::OnceLock::new();
        ENGINE.get_or_init(Arc::default).clone()
    }

    /// The compiled document of `tenant`.
    #[must_use]
    pub fn table(&self, tenant: &str) -> Option<Arc<Table>> {
        self.read().get(tenant).cloned()
    }

    // A writer holds the lock for one insert or remove of an `Arc`, which
    // leaves the map whole even if it panicked: a poisoned lock is read on.
    fn read(&self) -> RwLockReadGuard<'_, HashMap<String, Arc<Table>>> {
        self.tables.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn decide_now(&self, subject: &Subject, op: &Operation<'_>) -> Decision {
        let table = self.table(subject.tenant.as_str());
        decide::decide(
            table.as_deref(),
            &Caller::of(subject),
            op,
            chrono::Utc::now(),
        )
    }
}

impl PolicyEngine for JoinedcontextPolicy {
    fn name(&self) -> &str {
        POLICY_NAME
    }

    fn decide<'a>(&'a self, subject: &'a Subject, op: &'a Operation<'a>) -> DecisionFuture<'a> {
        Box::pin(std::future::ready(self.decide_now(subject, op)))
    }

    /// A notification is the answer to a query by the Subscription's
    /// stored subject, decided again at delivery: a revoked grant stops
    /// the next one.
    fn pre_notify(
        &self,
        subject: &Subject,
        _sub: &Value,
        _notification: &mut Value,
    ) -> NotifyDecision {
        match self.decide_now(subject, &Operation::new("5.7.2")) {
            Decision::Allow => NotifyDecision::Deliver,
            Decision::Filter(f) => NotifyDecision::Filter(f),
            Decision::Deny(_) | Decision::AllowIf(_) => NotifyDecision::Drop,
        }
    }

    fn loads_documents(&self) -> bool {
        true
    }

    fn load(&self, tenant: &str, document: &Value) -> Result<(), String> {
        let table = if document.is_null() {
            None
        } else {
            Some(Arc::new(document::compile(document)?))
        };
        let mut tables = self.tables.write().unwrap_or_else(PoisonError::into_inner);
        match table {
            Some(t) => tables.insert(tenant.to_owned(), t),
            None => tables.remove(tenant),
        };
        Ok(())
    }
}
