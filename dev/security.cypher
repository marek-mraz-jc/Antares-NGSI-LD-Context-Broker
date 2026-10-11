// routes-without-decision: an HTTP route whose handler reaches no policy decision (T-3569); public routes are allowlisted with their reason
MATCH (h:Symbol)-[:PROVIDES]->(c:Channel {channel:'http'}) WHERE NOT h.is_test AND NOT EXISTS { MATCH (h)-[:CALLS|DISPATCHES*1..6]->(g:Symbol) WHERE g.path IN ['antares_api::policy::gate','antares_api::policy::gate_each','antares_api::policy::decide','antares_api::policy::decide_each'] } RETURN c.key, h.path ORDER BY c.key;
// surface-calls-the-store: a surface symbol touching a store or SQL symbol, or AppState's store fields, past antares-ops (threat model 9.4 M2, F1)
MATCH (s:Symbol)-[:CALLS|REFERENCES]->(t:Symbol) WHERE NOT s.is_test AND s.package =~ 'antares-(views|odata|mcp|linkml|ws|icsr)' AND (t.package IN ['antares-store','antares-sql'] OR t.path IN ['antares_api::state::AppState::store','antares_api::state::AppState::temporal']) RETURN DISTINCT s.path, t.path ORDER BY s.path;
// protocol-crate-depends-on-antares: a protocol crate depending on an antares-* crate (threat model 9.4 M1)
MATCH (a:Package)-[d:DEPENDS_ON]->(b:Package) WHERE (a.name STARTS WITH 'odata-' OR a.name = 'linkml-model') AND b.name STARTS WITH 'antares-' RETURN a.name, b.name, d.kind;
// identity-headers: the Tenant header named outside the code that owns it (threat F2)
GREP (?i)"(x-)?ngsild-tenant" IN crates/*/src/**/*.rs;
// sql-from-text: dynamic SQL (sqlx takes a non-literal statement only through AssertSqlSafe); every function that builds one names why its interpolations are not input
MATCH (c:Symbol)-[:CALLS|REFERENCES]->(x:Symbol) WHERE x.name = 'AssertSqlSafe' AND x.external AND NOT c.is_test RETURN DISTINCT c.path ORDER BY c.path;
// egress-outside-policy: a reqwest client built anywhere but antares_jsonld::client_builder, which applies the EgressPolicy (SSRF)
MATCH (c:Symbol)-[:CALLS]->(x:Symbol) WHERE x.external AND x.path STARTS WITH 'reqwest::' AND x.name IN ['builder','new','default'] AND (x.path CONTAINS '::Client::' OR x.path CONTAINS '::ClientBuilder::') AND NOT c.is_test AND NOT c.file CONTAINS '/examples/' AND c.path <> 'antares_jsonld::loader::client_builder' RETURN DISTINCT c.path, x.path ORDER BY c.path;
