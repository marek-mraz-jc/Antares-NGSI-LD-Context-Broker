// SPDX-License-Identifier: EUPL-1.2
//! Who is calling, from the headers the gateway forwards (README section 1).
//! Values are read, never logged: the broker prints header names only.

use crate::document::{Assignee, Kind};
use antares_api::policy::Subject;
use std::collections::BTreeSet;

/// The headers this engine reads, in the order
/// `ANTARES_POLICY_SUBJECT_HEADERS` lists them.
pub const HEADERS: [&str; 5] = [
    "x-jc-subject",
    "x-jc-groups",
    "x-jc-roles",
    "x-jc-client",
    "x-jc-restricted",
];

/// The caller of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// No `x-jc-*` header at all: a call from inside the cluster.
    Internal,
    /// A caller the gateway verified, or an anonymous one.
    Platform(Platform),
}

/// A platform caller's identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Platform {
    /// `x-jc-subject`.
    pub user: Option<String>,
    /// `x-jc-client`.
    pub client: Option<String>,
    /// `x-jc-groups`.
    pub groups: BTreeSet<String>,
    /// `x-jc-roles`; exactly `public` for an anonymous caller.
    pub roles: BTreeSet<String>,
    /// `x-jc-restricted: true`: the caller wants a narrowed answer said.
    pub restricted: bool,
}

impl Caller {
    /// The caller `subject` carries.
    #[must_use]
    pub fn of(subject: &Subject) -> Self {
        let get = |name: &str| {
            subject
                .headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim())
        };
        if HEADERS.iter().all(|h| get(h).is_none()) {
            return Caller::Internal;
        }
        let one = |name| get(name).filter(|v| !v.is_empty()).map(str::to_owned);
        let list = |name| -> BTreeSet<String> {
            get(name)
                .into_iter()
                .flat_map(|v| v.split(','))
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
                .collect()
        };
        let (user, client) = (one("x-jc-subject"), one("x-jc-client"));
        let anonymous = user.is_none() && client.is_none();
        Caller::Platform(Platform {
            groups: if anonymous {
                BTreeSet::new()
            } else {
                list("x-jc-groups")
            },
            roles: if anonymous {
                BTreeSet::from(["public".to_owned()])
            } else {
                list("x-jc-roles")
            },
            restricted: get("x-jc-restricted").is_some_and(|v| v.eq_ignore_ascii_case("true")),
            user,
            client,
        })
    }
}

impl Platform {
    /// Whether `assignee` is this caller.
    #[must_use]
    pub fn is(&self, assignee: &Assignee) -> bool {
        let id = assignee.id.as_str();
        match assignee.kind {
            Kind::User => self.user.as_deref() == Some(id),
            Kind::Group => self.groups.contains(id),
            Kind::Role => self.roles.contains(id),
            Kind::ServiceAccount => self.client.as_deref() == Some(id),
            Kind::Did => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(headers: &[(&str, &str)]) -> Caller {
        Caller::of(&Subject {
            tenant: antares_model::TenantId::default(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        })
    }

    fn platform(c: Caller) -> Result<Platform, String> {
        match c {
            Caller::Platform(p) => Ok(p),
            Caller::Internal => Err("internal".into()),
        }
    }

    fn assignee(kind: Kind, id: &str) -> Assignee {
        Assignee {
            kind,
            id: id.to_owned(),
        }
    }

    #[test]
    fn no_header_is_internal_and_any_header_is_a_platform_caller() {
        assert_eq!(platform(caller(&[])), Err("internal".into()));
        assert_eq!(caller(&[("x-other", "1")]), Caller::Internal);
        assert!(matches!(caller(&[("X-JC-Roles", "")]), Caller::Platform(_)));
    }

    #[test]
    fn an_anonymous_caller_holds_public_and_nothing_it_claims() -> Result<(), String> {
        let p = platform(caller(&[("x-jc-roles", "admin"), ("x-jc-groups", "ops")]))?;
        assert_eq!(p.roles, BTreeSet::from(["public".to_owned()]));
        assert!(p.groups.is_empty());
        assert!(p.is(&assignee(Kind::Role, "public")));
        assert!(!p.is(&assignee(Kind::Role, "admin")));
        Ok(())
    }

    #[test]
    fn a_verified_caller_matches_by_each_kind() -> Result<(), String> {
        let c = caller(&[
            ("x-jc-subject", "ana@example.org"),
            ("x-jc-client", "app-1"),
            ("x-jc-groups", "ops, ,city"),
            ("x-jc-roles", "steward,endpoint:p/e"),
            ("x-jc-restricted", "TRUE"),
        ]);
        let p = platform(c)?;
        assert!(p.restricted);
        for (kind, id, yes) in [
            (Kind::User, "ana@example.org", true),
            (Kind::User, "bob@example.org", false),
            (Kind::ServiceAccount, "app-1", true),
            (Kind::ServiceAccount, "app-2", false),
            (Kind::Group, "city", true),
            (Kind::Group, "", false),
            (Kind::Role, "endpoint:p/e", true),
            (Kind::Role, "public", false),
            (Kind::Did, "did:web:x", false),
        ] {
            assert_eq!(p.is(&assignee(kind, id)), yes, "{kind:?} {id}");
        }
        let p = platform(caller(&[("x-jc-subject", "a"), ("x-jc-restricted", "yes")]))?;
        assert!(!p.restricted && p.roles.is_empty());
        Ok(())
    }
}
