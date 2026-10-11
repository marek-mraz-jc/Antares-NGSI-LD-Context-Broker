// SPDX-License-Identifier: EUPL-1.2
//! The view's EDM model (odata-linkml-mapping.md sections 3, 4 and 7.2):
//! the Tenant's `LinkML` schema intersected with what the view shows, built
//! once per request key and used for `$metadata` and for resolving every
//! name a request carries (M1 to M3). A type or an Attribute the view does
//! not show leaves no trace in it (S4).

use linkml_model::{edm_types, Builtin, Range, EDM_TYPE_TAG};
use odata_csdl::{
    Annotation, CsdlVersion, EntityContainer, EntitySet, EntityType, Expr, Include, Model,
    NavigationProperty, NavigationPropertyBinding, Property, Reference, Scale, Schema, Term,
    TypeRef,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::names::mangle;

/// The surface's own vocabulary (section 3.2): `Iri`, `Kind`, and the
/// abstract base every EntityType derives from.
pub const VOCABULARY: &str = "Antares.NgsiLd";
const VOCABULARY_ALIAS: &str = "NgsiLd";
const BASE: &str = "NgsiLd.Entity";

const VOCABULARIES: &str = "https://oasis-tcs.github.io/odata-vocabularies/vocabularies/";

/// The Entity types the view shows, each with the Attributes it shows, as
/// expanded IRIs.
pub type Shown = BTreeMap<String, BTreeSet<String>>;

/// The writes the view offers (its `operations`), which the Capabilities
/// annotations state (C2 to C5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Writable {
    /// `create_entity`: `POST {Set}` (W1).
    pub insert: bool,
    /// `merge_entity` or `replace_entity`: `PATCH` or `PUT` of an Entity
    /// (W4, W6).
    pub update: bool,
    /// `delete_entity` (W9).
    pub delete: bool,
}

/// How the surface reads and writes one member of an EntityType.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// A Property's `value` (P1, P6 when `collection`), typed by its range.
    Value {
        /// The Edm type of the value, or of each item.
        edm: &'static str,
        /// A ListProperty or a multivalued slot.
        collection: bool,
    },
    /// A GeoProperty (P3): `Edm.Geography`, never filtered as a value.
    Geo,
    /// A LanguageProperty (P4): one entry of the `languageMap`.
    Language,
    /// A VocabProperty (P5).
    Vocab,
    /// A JsonProperty or a value no range types (P7, `Edm.Untyped`).
    Json,
    /// A Relationship to a type the model does not hold (R4, R5): the
    /// `object` URI as a string.
    Url {
        /// A multivalued Relationship.
        collection: bool,
    },
    /// A Relationship to a visible type (R1 to R3).
    Nav {
        /// The EntitySet of the target type.
        target: String,
        /// A multivalued Relationship or a ListRelationship.
        collection: bool,
    },
}

impl Shape {
    /// Whether `$filter` may name it (C11).
    #[must_use]
    pub fn filterable(&self) -> bool {
        matches!(
            self,
            Self::Value { .. } | Self::Vocab | Self::Language | Self::Url { .. }
        )
    }

    /// Whether this round's `$filter` and `$orderby` take it: a
    /// single-valued value, the one form a `q` term compares today.
    #[must_use]
    pub fn scalar(&self) -> bool {
        matches!(
            self,
            Self::Value {
                collection: false,
                ..
            }
        )
    }

    /// Whether a write may name it: not a Relationship to a type the view
    /// hides (W15, it would link to an Entity the caller cannot see), not
    /// one entry of a `languageMap` (writing it would drop the others).
    #[must_use]
    pub fn writable(&self) -> bool {
        !matches!(self, Self::Url { .. } | Self::Language)
    }

    /// Whether `$orderby` may name it (C12).
    #[must_use]
    pub fn sortable(&self) -> bool {
        matches!(
            self,
            Self::Value {
                collection: false,
                ..
            } | Self::Vocab
                | Self::Url { collection: false }
        )
    }
}

/// One member of an EntityType: its NGSI-LD Attribute and its shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The Attribute's expanded IRI.
    pub iri: String,
    /// How it maps.
    pub shape: Shape,
    /// The slot is a `ListProperty` or a `ListRelationship`: a write
    /// keeps the kind (`valueList`, `objectList`).
    pub list: bool,
    /// When a client may write it (T-3722).
    pub written: Written,
}

/// When a client may write a member: `Core.Computed` never,
/// `Core.Immutable` on create only (T-3722).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Written {
    /// On create and on update.
    #[default]
    Always,
    /// On create only (`odata_immutable`).
    OnCreate,
    /// Never (`odata_computed`).
    Never,
}

/// One EntitySet: its Entity type and the members the view shows of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set {
    /// The Entity type's expanded IRI.
    pub type_iri: String,
    /// The members by their CSDL name.
    pub members: BTreeMap<String, Member>,
    /// The class's `identifier` slot, when the view shows it (E2a); the key
    /// is `id` without one (E2).
    pub identifier: Option<Identifier>,
    /// The writes the set takes: the view's, narrowed by the class (C10a).
    pub writable: Writable,
}

/// A key taken from the model (E2a): its value `v` names the Entity
/// `{prefix}{v}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier {
    /// The key member's CSDL name.
    pub member: String,
    /// Its Edm type: `Edm.String`, `Edm.Int64` or `Edm.Guid`.
    pub edm: &'static str,
    /// `urn:ngsi-ld:{Class}:`.
    pub prefix: String,
}

/// The view's model: the CSDL document and, for each EntitySet, how its
/// names map back to NGSI-LD.
#[derive(Debug, Clone, PartialEq)]
pub struct Edm {
    /// The CSDL model `$metadata` writes and requests resolve against.
    pub csdl: Model,
    /// The view's namespace.
    pub namespace: String,
    /// The EntitySets by name.
    pub sets: BTreeMap<String, Set>,
}

/// Builds the model of view `view` from `schema` and what the view shows.
/// A class the view does not show, a slot it does not show of that class,
/// and a name with no CSDL form are left out; a Relationship whose target
/// is not visible becomes a URL string (R5).
///
/// # Errors
/// When the view id has no CSDL namespace form.
pub fn build(
    view: &str,
    schema: &linkml_model::Model,
    shown: &Shown,
    writable: Writable,
    version: CsdlVersion,
) -> Result<Edm, String> {
    let namespace = mangle(view).ok_or("the view id has no OData namespace form")?;
    let mut visible: BTreeMap<&str, (String, &BTreeSet<String>)> = BTreeMap::new();
    for class in schema.schema().classes.keys() {
        let (Some(attrs), Some(name)) = (shown.get(&schema.class_iri(class)), mangle(class)) else {
            continue;
        };
        visible.insert(class.as_str(), (name, attrs));
    }
    let mut types = Vec::new();
    let mut sets = BTreeMap::new();
    let mut container = EntityContainer::new("Container");
    for (class, (name, attrs)) in &visible {
        let mut ty = EntityType::new(name.clone());
        ty.base_type = Some(BASE.into());
        let mut identifier = None;
        let mut required = Vec::new();
        ty.annotations.push(iri(&schema.class_iri(class)));
        let own = schema.schema().classes.get(*class);
        ty.annotations
            .extend(own.and_then(|c| c.description.as_deref()).map(description));
        let writable = narrowed(writable, own);
        let mut set = EntitySet::new(name.clone(), format!("{namespace}.{name}"));
        let mut members = BTreeMap::new();
        for (slot_name, slot) in schema.class_slots(class) {
            let slot_iri = schema.slot_iri(slot_name);
            let Some(member) = attrs
                .contains(&slot_iri)
                .then(|| mangle(slot_name))
                .flatten()
            else {
                continue;
            };
            let shape = shape(schema, &slot, &visible);
            if version == CsdlVersion::V4_0 && shape == Shape::Json {
                // CSDL XML 3.4: a 4.0 client gets no Untyped property and an open type.
                ty.open = true;
                continue;
            }
            if let (Some(true), Shape::Value { edm, .. }) = (slot.identifier, &shape) {
                identifier = Some(keyed(&mut ty, class, &member, edm));
            }
            match &shape {
                Shape::Nav { target, collection } => {
                    let ordered = schema.kind(&slot) == "ListRelationship";
                    let target_ty = format!("{namespace}.{target}");
                    ty.navigation_properties.push(navigation(
                        &member,
                        &slot_iri,
                        target_ty,
                        *collection,
                        ordered,
                    ));
                    set.bindings.push(NavigationPropertyBinding {
                        path: member.clone(),
                        target: target.clone(),
                    });
                }
                other => {
                    let p = property(&member, &slot_iri, other, schema, &slot);
                    ty.properties
                        .push(key_property(p, ty.key.contains(&member), other));
                    let named = slot.required == Some(true);
                    required.extend(named.then(|| Expr::PropertyPath(member.clone())));
                }
            }
            members.insert(
                member,
                Member {
                    iri: slot_iri,
                    shape,
                    list: schema.kind(&slot).starts_with("List"),
                    written: written(&slot),
                },
            );
        }
        set.annotations = capabilities(&members, writable, required);
        container.entity_sets.push(set);
        types.push(ty);
        sets.insert(
            name.clone(),
            Set {
                type_iri: schema.class_iri(class),
                members,
                identifier,
                writable,
            },
        );
    }
    container.annotations = container_capabilities();
    let mut own = Schema::new(namespace.clone());
    own.entity_types = types;
    own.entity_container = Some(container);
    Ok(Edm {
        csdl: Model {
            version,
            references: references(),
            schemas: vec![vocabulary(), own],
        },
        namespace,
        sets,
    })
}

/// A key member is required, and one a create may leave out is
/// `Core.ComputedDefaultValue`: the surface mints a uuid for a `Guid` or
/// `String` key (W1).
fn key_property(p: Property, key: bool, shape: &Shape) -> Property {
    if !key {
        return p;
    }
    let mut p = p.required();
    if matches!(
        shape,
        Shape::Value {
            edm: "Edm.Guid" | "Edm.String",
            ..
        }
    ) {
        p.annotations.push(core("ComputedDefaultValue"));
    }
    p
}

/// E2a: `ty` declares the base members itself and is keyed by `member`.
fn keyed(ty: &mut EntityType, class: &str, member: &str, edm: &'static str) -> Identifier {
    ty.base_type = None;
    ty.key = vec![member.to_owned()];
    ty.properties.splice(0..0, base_properties());
    Identifier {
        member: member.to_owned(),
        edm,
        prefix: format!("urn:ngsi-ld:{class}:"),
    }
}

/// Rows R1 to R4: the navigation property of a Relationship slot.
fn navigation(
    member: &str,
    slot_iri: &str,
    target_ty: String,
    collection: bool,
    ordered: bool,
) -> NavigationProperty {
    let ty = if collection {
        TypeRef::collection(target_ty)
    } else {
        TypeRef::single(target_ty)
    };
    let mut nav = NavigationProperty::new(member, ty);
    // CSDL XML 8.1.4: a collection-valued navigation is never null.
    nav.nullable = !collection;
    nav.annotations.push(iri(slot_iri));
    if ordered {
        nav.annotations.push(core("Ordered"));
    }
    nav
}

/// Section 3.4 and rows P1 to P7, R1 to R5.
fn shape(
    schema: &linkml_model::Model,
    slot: &linkml_model::Slot,
    visible: &BTreeMap<&str, (String, &BTreeSet<String>)>,
) -> Shape {
    let many = slot.multivalued == Some(true);
    match (schema.kind(slot), schema.range(slot)) {
        // P2: an inlined class is a JSON object value, whatever kind is named.
        (_, Range::Class(_)) if slot.inlined == Some(true) => Shape::Json,
        ("Relationship" | "ListRelationship", Range::Class(c)) => {
            let collection = many || schema.kind(slot) == "ListRelationship";
            match visible.get(c) {
                Some((target, _)) => Shape::Nav {
                    target: target.clone(),
                    collection,
                },
                None => Shape::Url { collection },
            }
        }
        ("Relationship" | "ListRelationship", _) => Shape::Url {
            collection: many || schema.kind(slot) == "ListRelationship",
        },
        ("GeoProperty", _) => Shape::Geo,
        ("LanguageProperty", _) => Shape::Language,
        ("VocabProperty", _) => Shape::Vocab,
        ("JsonProperty", _) | (_, Range::Class(_)) => Shape::Json,
        (kind, Range::Enum(_)) => Shape::Value {
            edm: "Edm.String",
            collection: many || kind == "ListProperty",
        },
        (kind, Range::Type(b)) => Shape::Value {
            // The model admits only the narrowings of `edm_types`.
            edm: slot
                .annotations
                .get(EDM_TYPE_TAG)
                .and_then(|a| edm_types(b).iter().find(|t| **t == a.value))
                .copied()
                .unwrap_or_else(|| edm_type(b)),
            collection: many || kind == "ListProperty",
        },
    }
}

/// Section 3.4.
fn edm_type(b: Builtin) -> &'static str {
    match b {
        Builtin::String | Builtin::Uri | Builtin::Uriorcurie => "Edm.String",
        Builtin::Integer => "Edm.Int64",
        Builtin::Float | Builtin::Double => "Edm.Double",
        Builtin::Decimal => "Edm.Decimal",
        Builtin::Boolean => "Edm.Boolean",
        Builtin::Date => "Edm.Date",
        Builtin::Datetime => "Edm.DateTimeOffset",
        Builtin::Time => "Edm.TimeOfDay",
    }
}

fn property(
    name: &str,
    slot_iri: &str,
    shape: &Shape,
    schema: &linkml_model::Model,
    slot: &linkml_model::Slot,
) -> Property {
    let (ty, collection) = match shape {
        Shape::Value { edm, collection } => (*edm, *collection),
        Shape::Geo => ("Edm.Geography", false),
        Shape::Language | Shape::Vocab => ("Edm.String", false),
        Shape::Url { collection } => ("Edm.String", *collection),
        Shape::Json | Shape::Nav { .. } => ("Edm.Untyped", false),
    };
    let mut p = Property::new(
        name,
        if collection {
            TypeRef::collection(ty)
        } else {
            TypeRef::single(ty)
        },
    );
    if ty == "Edm.Decimal" {
        p.scale = Some(Scale::Variable);
    }
    p.annotations.push(iri(slot_iri));
    p.annotations.push(Annotation::new(
        format!("{VOCABULARY_ALIAS}.Kind"),
        Expr::String(schema.kind(slot).to_owned()),
    ));
    let range = schema.range(slot);
    if matches!(shape, Shape::Url { .. })
        || matches!(range, Range::Type(Builtin::Uri | Builtin::Uriorcurie))
    {
        p.annotations.push(core("IsURL"));
    }
    if *shape == Shape::Language {
        p.annotations.push(core("IsLanguageDependent"));
    }
    if schema.kind(slot) == "ListProperty" {
        p.annotations.push(core("Ordered"));
    }
    match written(slot) {
        Written::Never => p.annotations.push(core("Computed")),
        Written::OnCreate => p.annotations.push(core("Immutable")),
        Written::Always => {}
    }
    p.annotations
        .extend(slot.description.as_deref().map(description));
    if let Range::Enum(e) = range {
        // `range` names an enum only when the schema holds it.
        let values = schema.schema().enums.get(e).into_iter();
        p.annotations.push(Annotation::new(
            "Validation.AllowedValues",
            Expr::Collection(
                values
                    .flat_map(|v| v.permissible_values.keys())
                    .map(|v| Expr::Record(vec![("Value".into(), Expr::String(v.clone()))]))
                    .collect(),
            ),
        ));
    }
    p
}

fn iri(value: &str) -> Annotation {
    Annotation::new(
        format!("{VOCABULARY_ALIAS}.Iri"),
        Expr::String(value.to_owned()),
    )
}

fn core(term: &str) -> Annotation {
    Annotation::new(format!("Core.{term}"), Expr::Bool(true))
}

/// The view's writes narrowed by the class's `odata_insertable`,
/// `odata_updatable` and `odata_deletable` (C10a): `"false"` takes one away,
/// nothing adds one.
fn narrowed(writable: Writable, class: Option<&linkml_model::Class>) -> Writable {
    let flag = |tag: &str| {
        class
            .and_then(|c| c.annotations.get(tag))
            .is_none_or(|a| a.value != "false")
    };
    Writable {
        insert: writable.insert && flag("odata_insertable"),
        update: writable.update && flag("odata_updatable"),
        delete: writable.delete && flag("odata_deletable"),
    }
}

/// `Core.Description` of a slot or a class (T-3722).
fn description(text: &str) -> Annotation {
    Annotation::new("Core.Description", Expr::String(text.to_owned()))
}

/// When a client may write the slot's member (T-3722); the model admits
/// only `"true"` and `"false"` as the flags' values.
fn written(slot: &linkml_model::Slot) -> Written {
    let set = |tag: &str| slot.annotations.get(tag).is_some_and(|a| a.value == "true");
    if set("odata_computed") {
        Written::Never
    } else if set("odata_immutable") {
        Written::OnCreate
    } else {
        Written::Always
    }
}

/// C2 to C16: the writes the view offers, none of them deep or an upsert,
/// with the members no write may name ([`Shape::writable`], the computed
/// ones are `Core.Computed`); counting; filtering and sorting on the
/// members [`Shape::scalar`] admits and `id` (C11, C12); expanding one
/// level (C13). Advisory: every request is still gated (O25).
fn capabilities(
    members: &BTreeMap<String, Member>,
    writable: Writable,
    required: Vec<Expr>,
) -> Vec<Annotation> {
    let flag = |value: bool| Expr::Bool(value);
    let restriction = |term: &str, record: Vec<(&str, Expr)>| {
        Annotation::new(
            format!("Capabilities.{term}"),
            Expr::Record(record.into_iter().map(|(m, v)| (m.into(), v)).collect()),
        )
    };
    let unserved: Vec<Expr> = ["_types", "scope", "createdAt", "modifiedAt"]
        .into_iter()
        .map(str::to_owned)
        .chain(
            members
                .iter()
                .filter(|(_, m)| !m.shape.scalar() && !matches!(m.shape, Shape::Nav { .. }))
                .map(|(name, _)| name.clone()),
        )
        .map(Expr::PropertyPath)
        .collect();
    let refused = |create: bool| -> Vec<Expr> {
        members
            .iter()
            .filter(|(_, m)| {
                !m.shape.writable()
                    || m.written == Written::Never
                    || (!create && m.written == Written::OnCreate)
            })
            .map(|(name, _)| Expr::PropertyPath(name.clone()))
            .collect()
    };
    vec![
        restriction(
            "InsertRestrictions",
            vec![
                ("Insertable", flag(writable.insert)),
                ("NonInsertableProperties", Expr::Collection(refused(true))),
                // C5: a create names them (LinkML `required`)
                ("RequiredProperties", Expr::Collection(required)),
            ],
        ),
        restriction(
            "UpdateRestrictions",
            vec![
                ("Updatable", flag(writable.update)),
                ("Upsertable", flag(false)),
                ("DeltaUpdateSupported", flag(false)),
                ("NonUpdatableProperties", Expr::Collection(refused(false))),
            ],
        ),
        restriction(
            "DeleteRestrictions",
            vec![("Deletable", flag(writable.delete))],
        ),
        restriction("CountRestrictions", vec![("Countable", flag(true))]),
        restriction("SearchRestrictions", vec![("Searchable", flag(false))]),
        restriction(
            "FilterRestrictions",
            vec![
                ("Filterable", flag(true)),
                ("MaxLevels", Expr::Int(1)),
                (
                    "NonFilterableProperties",
                    Expr::Collection(unserved.clone()),
                ),
            ],
        ),
        restriction(
            "SortRestrictions",
            vec![
                ("Sortable", flag(true)),
                ("NonSortableProperties", Expr::Collection(unserved)),
            ],
        ),
        restriction(
            "ExpandRestrictions",
            vec![("Expandable", flag(true)), ("MaxLevels", Expr::Int(1))],
        ),
        restriction("DeepInsertSupport", vec![("Supported", flag(false))]),
        restriction("DeepUpdateSupport", vec![("Supported", flag(false))]),
    ]
}

/// C18 to C20.
fn container_capabilities() -> Vec<Annotation> {
    let flag = |term: &str, value: bool| {
        Annotation::new(format!("Capabilities.{term}"), Expr::Bool(value))
    };
    vec![
        Annotation::new(
            "Capabilities.ConformanceLevel",
            Expr::EnumMember {
                ty: "Capabilities.ConformanceLevelType".into(),
                members: vec!["Minimal".into()],
            },
        ),
        // C18: the surface serves `$batch` (T-3723); Content-ID references are 501.
        flag("BatchSupported", true),
        Annotation::new(
            "Capabilities.BatchSupport",
            Expr::Record(vec![
                ("Supported".into(), Expr::Bool(true)),
                ("ContinueOnErrorSupported".into(), Expr::Bool(true)),
                (
                    "ReferencesInRequestBodiesSupported".into(),
                    Expr::Bool(false),
                ),
                (
                    "ReferencesAcrossChangeSetsSupported".into(),
                    Expr::Bool(false),
                ),
                (
                    "SupportedFormats".into(),
                    Expr::Collection(vec![
                        Expr::String("multipart/mixed".into()),
                        Expr::String("application/json".into()),
                    ]),
                ),
            ]),
        ),
        flag("QuerySegmentSupported", true),
        flag("CrossJoinSupported", false),
        flag("ComputeSupported", false),
        flag("KeyAsSegmentSupported", false),
        flag("AsynchronousRequestsSupported", false),
    ]
}

fn references() -> Vec<Reference> {
    ["Core", "Capabilities", "Validation"]
        .into_iter()
        .map(|alias| Reference {
            uri: format!("{VOCABULARIES}Org.OData.{alias}.V1.xml"),
            includes: vec![Include {
                namespace: format!("Org.OData.{alias}.V1"),
                alias: Some(alias.into()),
            }],
        })
        .collect()
}

/// `Antares.NgsiLd`: the terms the surface annotates with and the abstract
/// base that holds the key (E2), the types (E3), the scope (E5) and the
/// system members (E6).
fn vocabulary() -> Schema {
    let mut v = Schema::new(VOCABULARY);
    v.alias = Some(VOCABULARY_ALIAS.into());
    v.terms = vec![
        Term::new("Iri", TypeRef::single("Edm.String")),
        Term::new("Kind", TypeRef::single("Edm.String")),
        Term::new("observedAt", TypeRef::single("Edm.DateTimeOffset")),
        Term::new("datasetId", TypeRef::single("Edm.String")),
    ];
    let mut base = EntityType::new("Entity");
    base.is_abstract = true;
    base.key = vec!["id".into()];
    base.properties = base_properties();
    v.entity_types = vec![base];
    v
}

/// The members of `NgsiLd.Entity` (E2, E3, E5, E6).
fn base_properties() -> Vec<Property> {
    let computed = |p: Property| {
        let mut p = p.required();
        p.annotations.push(core("Computed"));
        p
    };
    // W1: a create that leaves out `id` gets one minted
    let mut id = Property::new("id", TypeRef::single("Edm.String")).required();
    id.annotations.push(core("ComputedDefaultValue"));
    vec![
        id,
        computed(Property::new("_types", TypeRef::collection("Edm.String"))),
        Property::new("scope", TypeRef::collection("Edm.String")).required(),
        computed(Property::new(
            "createdAt",
            TypeRef::single("Edm.DateTimeOffset"),
        )),
        computed(Property::new(
            "modifiedAt",
            TypeRef::single("Edm.DateTimeOffset"),
        )),
    ]
}

/// The members every EntityType inherits from the base, as the surface
/// reads them.
#[must_use]
pub fn base_member(name: &str) -> bool {
    matches!(name, "id" | "_types" | "scope" | "createdAt" | "modifiedAt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> linkml_model::Model {
        let json = br#"{
          "id": "https://example.org/city", "name": "city",
          "prefixes": {"city": {"prefix_prefix": "city", "prefix_reference": "https://example.org/city/"}},
          "default_prefix": "city",
          "classes": {
            "Car": {"slots": ["speed", "plate", "owner", "secret", "parkedIn", "colour", "notes", "spot", "label", "tags", "price", "photo",
                              "drivers", "route", "maker", "trailer", "service", "ratings", "shades", "palette", "kind", "dims", "stops", "makers"]},
            "Person": {"slots": ["name"]},
            "Garage": {"slots": ["name"]}
          },
          "slots": {
            "speed": {"range": "float"},
            "plate": {},
            "owner": {"range": "Person"},
            "secret": {"range": "integer"},
            "parkedIn": {"range": "Garage", "multivalued": true},
            "colour": {"range": "Colour"},
            "notes": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "JsonProperty"}}},
            "spot": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "GeoProperty"}}},
            "label": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "LanguageProperty"}}},
            "tags": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "ListProperty"}}},
            "price": {"range": "decimal"},
            "photo": {"range": "uri"},
            "name": {},
            "drivers": {"range": "Person", "multivalued": true},
            "route": {"range": "Person", "annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "ListRelationship"}}},
            "maker": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "Relationship"}}},
            "trailer": {"range": "Person", "inlined": true, "annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "Relationship"}}},
            "service": {"range": "Person", "inlined": true},
            "ratings": {"range": "integer", "multivalued": true},
            "shades": {"range": "Colour", "multivalued": true},
            "palette": {"range": "Colour", "annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "ListProperty"}}},
            "kind": {"range": "Colour", "annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "VocabProperty"}}},
            "dims": {"range": "boolean"},
            "stops": {"annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "ListRelationship"}}},
            "makers": {"multivalued": true, "annotations": {"ngsi_ld_kind": {"tag": "ngsi_ld_kind", "value": "Relationship"}}}
          },
          "enums": {"Colour": {"permissible_values": {"red": {}, "pm2.5": {}}}}
        }"#;
        let limits = linkml_model::Limits {
            max_bytes: 1 << 16,
            max_depth: 16,
            max_elements: 1024,
        };
        linkml_model::Model::read(json, &limits).unwrap()
    }

    fn shown(s: &linkml_model::Model, types: &[(&str, &[&str])]) -> Shown {
        types
            .iter()
            .map(|(t, attrs)| {
                (
                    s.class_iri(t),
                    attrs.iter().map(|a| s.slot_iri(a)).collect(),
                )
            })
            .collect()
    }

    const CAR_ALL: &[&str] = &[
        "speed", "plate", "owner", "parkedIn", "colour", "notes", "spot", "label", "tags", "price",
        "photo", "drivers", "route", "maker", "trailer", "service", "ratings", "shades", "palette",
        "kind", "dims", "stops", "makers",
    ];

    fn xml(edm: &Edm) -> String {
        let mut out = Vec::new();
        odata_csdl::write_xml(&edm.csdl, &odata_csdl::Limits::strict(), &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_model_is_valid_csdl_in_both_versions() {
        let s = schema();
        let shown = shown(&s, &[("Car", CAR_ALL), ("Person", &["name"])]);
        for version in [CsdlVersion::V4_01, CsdlVersion::V4_0] {
            let edm = build("city-cars", &s, &shown, Writable::default(), version).unwrap();
            odata_csdl::validate(&edm.csdl, &odata_csdl::Limits::strict()).unwrap();
            let mut json = Vec::new();
            if version == CsdlVersion::V4_01 {
                odata_csdl::write_json(&edm.csdl, &odata_csdl::Limits::strict(), &mut json)
                    .unwrap();
            }
        }
    }

    #[test]
    fn each_slot_maps_by_its_kind_and_range() {
        let s = schema();
        let edm = build(
            "v",
            &s,
            &shown(&s, &[("Car", CAR_ALL), ("Person", &["name"])]),
            Writable::default(),
            CsdlVersion::V4_01,
        )
        .unwrap();
        let car = &edm.sets["Car"].members;
        let shape = |n: &str| car[n].shape.clone();
        assert_eq!(
            shape("speed"),
            Shape::Value {
                edm: "Edm.Double",
                collection: false
            }
        );
        assert_eq!(
            shape("plate"),
            Shape::Value {
                edm: "Edm.String",
                collection: false
            }
        );
        assert_eq!(
            shape("owner"),
            Shape::Nav {
                target: "Person".into(),
                collection: false
            },
            "R1: a visible target"
        );
        assert_eq!(
            shape("parkedIn"),
            Shape::Url { collection: true },
            "R5: Garage is hidden"
        );
        assert_eq!(
            shape("colour"),
            Shape::Value {
                edm: "Edm.String",
                collection: false
            }
        );
        assert_eq!(shape("notes"), Shape::Json);
        assert_eq!(shape("spot"), Shape::Geo);
        assert_eq!(shape("label"), Shape::Language);
        assert_eq!(
            shape("tags"),
            Shape::Value {
                edm: "Edm.String",
                collection: true
            }
        );
        assert_eq!(car["speed"].iri, s.slot_iri("speed"));
        let collection = |target: &str| Shape::Nav {
            target: target.into(),
            collection: true,
        };
        assert_eq!(shape("drivers"), collection("Person"), "R2");
        assert_eq!(shape("route"), collection("Person"), "R3");
        assert_eq!(
            shape("maker"),
            Shape::Url { collection: false },
            "R4: no target class"
        );
        assert_eq!(
            shape("trailer"),
            Shape::Json,
            "an inlined Relationship is its value"
        );
        assert_eq!(shape("service"), Shape::Json, "P2: an inlined class");
        let value = |edm, collection| Shape::Value { edm, collection };
        assert_eq!(shape("ratings"), value("Edm.Int64", true));
        assert_eq!(shape("shades"), value("Edm.String", true));
        assert_eq!(shape("palette"), value("Edm.String", true));
        assert_eq!(shape("kind"), Shape::Vocab);
        assert_eq!(
            shape("makers"),
            Shape::Url { collection: true },
            "R4, multivalued"
        );
        assert_eq!(
            shape("stops"),
            Shape::Url { collection: true },
            "R4 of a list"
        );
        assert_eq!(shape("dims"), value("Edm.Boolean", false));
        let x = xml(&edm);
        assert!(
            x.contains(r#"<NavigationPropertyBinding Path="owner" Target="Person"/>"#),
            "{x}"
        );
        assert!(x.contains(r#"Scale="variable""#), "{x}");
        assert!(x.contains(r#"Property="Value" String="pm2.5""#), "{x}");
        assert!(x.contains(r#"Term="Core.IsURL""#), "{x}");
        assert!(x.contains(r#"Term="Core.IsLanguageDependent""#), "{x}");
        assert!(x.contains(r#"Term="Core.Ordered""#), "{x}");
    }

    #[test]
    fn a_hidden_type_or_attribute_leaves_no_trace() {
        let s = schema();
        let edm = build(
            "v",
            &s,
            &shown(&s, &[("Car", &["speed", "parkedIn"])]),
            Writable::default(),
            CsdlVersion::V4_01,
        )
        .unwrap();
        let x = xml(&edm);
        for hidden in [
            "secret", "Person", "Garage", "owner", "plate", "/secret", "/Garage",
        ] {
            assert!(!x.contains(hidden), "{hidden} in {x}");
        }
        assert_eq!(edm.sets.keys().collect::<Vec<_>>(), vec!["Car"]);
    }

    #[test]
    fn a_4_0_model_has_no_untyped_property_and_an_open_type() {
        let s = schema();
        let edm = build(
            "v",
            &s,
            &shown(&s, &[("Car", CAR_ALL)]),
            Writable::default(),
            CsdlVersion::V4_0,
        )
        .unwrap();
        assert!(!edm.sets["Car"].members.contains_key("notes"));
        assert!(!xml(&edm).contains("Edm.Untyped"));
        assert!(xml(&edm).contains(r#"OpenType="true""#));
    }

    #[test]
    fn names_are_mangled_and_a_view_id_without_a_namespace_form_is_refused() {
        let s = schema();
        let edm = build(
            "city-cars",
            &s,
            &shown(&s, &[("Car", &["speed"])]),
            Writable::default(),
            CsdlVersion::V4_01,
        )
        .unwrap();
        assert_eq!(edm.namespace, "city_x002D_cars");
        assert!(build(
            &"v".repeat(200),
            &s,
            &Shown::new(),
            Writable::default(),
            CsdlVersion::V4_01
        )
        .is_err());
    }

    #[test]
    fn every_builtin_has_its_edm_type() {
        use linkml_model::Builtin as B;
        let all = [
            (B::String, "Edm.String"),
            (B::Uri, "Edm.String"),
            (B::Uriorcurie, "Edm.String"),
            (B::Integer, "Edm.Int64"),
            (B::Float, "Edm.Double"),
            (B::Double, "Edm.Double"),
            (B::Decimal, "Edm.Decimal"),
            (B::Boolean, "Edm.Boolean"),
            (B::Date, "Edm.Date"),
            (B::Datetime, "Edm.DateTimeOffset"),
            (B::Time, "Edm.TimeOfDay"),
        ];
        for (b, edm) in all {
            assert_eq!(edm_type(b), edm, "{b:?}");
        }
    }

    #[test]
    fn shapes_say_what_filters_and_sorts() {
        assert!(Shape::Value {
            edm: "Edm.Int64",
            collection: false
        }
        .sortable());
        assert!(!Shape::Value {
            edm: "Edm.Int64",
            collection: true
        }
        .sortable());
        assert!(Shape::Value {
            edm: "Edm.Int64",
            collection: true
        }
        .filterable());
        assert!(!Shape::Json.filterable());
        assert!(!Shape::Geo.filterable());
        assert!(!Shape::Geo.sortable());
        assert!(!Shape::Nav {
            target: "A".into(),
            collection: false
        }
        .filterable());
        assert!(Shape::Url { collection: false }.sortable());
        assert!(base_member("_types"));
        assert!(!base_member("speed"));
    }
}
