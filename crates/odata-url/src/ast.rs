// SPDX-License-Identifier: EUPL-1.2
//! The typed result of a parse. Names stay strings: the [`Model`](crate::Model) placed each one,
//! the caller decides what it means.

/// One relative OData URL.
///
/// ```
/// use odata_url::{ODataUri, QueryOptions};
/// assert_eq!(ODataUri::ServiceDocument.to_string(), "");
/// assert_eq!(ODataUri::Metadata { options: QueryOptions::default() }.to_string(), "$metadata");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ODataUri {
    /// The empty relative URL: the service document.
    ServiceDocument,
    /// `$metadata`, with `$format` and custom options only.
    Metadata {
        /// Its query options.
        options: QueryOptions,
    },
    /// `$batch`, with `$format` and custom options only.
    Batch {
        /// Its query options.
        options: QueryOptions,
    },
    /// `$entity?$id=...`; [`QueryOptions::id`] holds the id, as written.
    Entity {
        /// Its query options.
        options: QueryOptions,
    },
    /// A resource path with its query options.
    Resource {
        /// The segments, in order.
        path: Vec<Segment>,
        /// Its query options.
        options: QueryOptions,
    },
}

/// One resource path segment.
///
/// ```
/// use odata_url::{ODataUri, QueryOptions, Segment};
/// let path = vec![Segment::EntitySet("Buildings".into()), Segment::Count];
/// assert_eq!(ODataUri::Resource { path, options: QueryOptions::default() }.to_string(), "Buildings/$count");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Segment {
    /// An entity set at the service root.
    EntitySet(String),
    /// A key predicate after a collection of entities.
    Key(Key),
    /// A structural property: primitive, complex, or a collection of either.
    Property(String),
    /// A navigation property.
    Navigation(String),
    /// `/$count`.
    Count,
    /// `/$ref`.
    Ref,
    /// `/$value` of a primitive property.
    Value,
    /// `/$query`.
    Query,
}

/// A key predicate.
///
/// ```
/// use odata_url::{Key, KeyValue, Literal, Segment};
/// let key = Key::Compound(vec![("id".into(), KeyValue::Literal(Literal::Int(1)))]);
/// assert_eq!(Segment::Key(key).to_string(), "(id=1)");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Key {
    /// `(value)`.
    Single(KeyValue),
    /// `(name=value,...)`.
    Compound(Vec<(String, KeyValue)>),
}

/// The value of a key property.
///
/// ```
/// use odata_url::KeyValue;
/// assert_eq!(KeyValue::Alias("k".into()).to_string(), "@k");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum KeyValue {
    /// A literal.
    Literal(Literal),
    /// A parameter alias, `@name` (without the `@`).
    Alias(String),
}

/// System, alias and custom query options. Each system option appears at most once.
///
/// ```
/// use odata_url::{ODataUri, QueryOptions};
/// let options = QueryOptions { top: Some(5), skip: Some(10), ..QueryOptions::default() };
/// assert_eq!(ODataUri::Batch { options }.to_string(), "$batch?$top=5&$skip=10");
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct QueryOptions {
    /// `$filter`.
    pub filter: Option<Expr>,
    /// `$select`.
    pub select: Option<Vec<SelectItem>>,
    /// `$expand`.
    pub expand: Option<Vec<ExpandItem>>,
    /// `$orderby`.
    pub orderby: Option<Vec<OrderBy>>,
    /// `$top`.
    pub top: Option<u64>,
    /// `$skip`.
    pub skip: Option<u64>,
    /// `$count`.
    pub count: Option<bool>,
    /// `$format`.
    pub format: Option<Format>,
    /// `$skiptoken`, as written.
    pub skiptoken: Option<String>,
    /// `$schemaversion`, as written (`*` or a version).
    pub schemaversion: Option<String>,
    /// `$id`, as written.
    pub id: Option<String>,
    /// Parameter aliases bound to literals, `@name=value` (names without the `@`).
    pub aliases: Vec<(String, Literal)>,
    /// Custom query options, as written: name and value after `=` (URL Conventions 5.2).
    pub custom: Vec<(String, Option<String>)>,
}

/// One `$select` item.
///
/// ```
/// use odata_url::SelectItem;
/// assert_eq!(SelectItem::Path(vec!["address".into(), "city".into()]).to_string(), "address/city");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`.
    Star,
    /// A property path: a structural or navigation property, through complex properties.
    Path(Vec<String>),
    /// An annotation, `@namespace.term[#qualifier]`, written with its `@`.
    Annotation(String),
}

/// One `$expand` item.
///
/// ```
/// use odata_url::{ExpandItem, ExpandKind, ExpandPath, Levels};
/// let item = ExpandItem {
///     path: ExpandPath::Path(vec!["sensors".into()]),
///     kind: ExpandKind::Entities { select: None, expand: None, levels: Some(Levels::Max) },
/// };
/// assert_eq!(item.to_string(), "sensors($levels=max)");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandItem {
    /// What it expands.
    pub path: ExpandPath,
    /// References or the related entities themselves.
    pub kind: ExpandKind,
}

/// What an [`ExpandItem`] expands.
///
/// ```
/// use odata_url::{ExpandItem, ExpandKind, ExpandPath};
/// assert_eq!(ExpandItem { path: ExpandPath::Star, kind: ExpandKind::Ref }.to_string(), "*/$ref");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ExpandPath {
    /// `*`: every navigation property.
    Star,
    /// A navigation property, through complex properties.
    Path(Vec<String>),
}

/// How an [`ExpandItem`] expands.
///
/// ```
/// use odata_url::{ExpandItem, ExpandKind, ExpandPath};
/// let kind = ExpandKind::Entities { select: None, expand: None, levels: None };
/// assert_eq!(ExpandItem { path: ExpandPath::Path(vec!["owner".into()]), kind }.to_string(), "owner");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum ExpandKind {
    /// `/$ref`: entity references only.
    Ref,
    /// The related entities, with the nested options of the subset.
    Entities {
        /// Nested `$select`.
        select: Option<Vec<SelectItem>>,
        /// Nested `$expand`.
        expand: Option<Vec<ExpandItem>>,
        /// `$levels`.
        levels: Option<Levels>,
    },
}

/// `$levels`.
///
/// ```
/// use odata_url::Levels;
/// assert_eq!(Levels::Count(2).to_string(), "2");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Levels {
    /// A number of levels, at least 1.
    Count(u64),
    /// `max`.
    Max,
}

/// One `$orderby` item.
///
/// ```
/// use odata_url::{Expr, MemberPath, OrderBy};
/// let expr = Expr::Member(MemberPath { variable: None, path: vec!["height".into()] });
/// assert_eq!(OrderBy { expr, descending: true }.to_string(), "height%20desc");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    /// What it sorts by.
    pub expr: Expr,
    /// `desc`.
    pub descending: bool,
}

/// `$format`.
///
/// ```
/// use odata_url::Format;
/// assert_eq!(Format::MediaType("application/json".into()).to_string(), "application/json");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Format {
    /// `json`.
    Json,
    /// `xml`.
    Xml,
    /// `atom`.
    Atom,
    /// A media type with its parameters, as written.
    MediaType(String),
}

/// A `$filter` or `$orderby` expression.
///
/// ```
/// use odata_url::{CompareOp, Expr, Literal, MemberPath};
/// let height = Box::new(Expr::Member(MemberPath { variable: None, path: vec!["height".into()] }));
/// let tall = Expr::Compare(height, CompareOp::Gt, Box::new(Expr::Literal(Literal::Int(10))));
/// assert_eq!(Expr::Not(Box::new(tall)).to_string(), "not%20(height%20gt%2010)");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A literal.
    Literal(Literal),
    /// A parameter alias, `@name` (without the `@`).
    Alias(String),
    /// A member path.
    Member(MemberPath),
    /// `left op right`.
    Compare(Box<Expr>, CompareOp, Box<Expr>),
    /// `left in (literal, ...)`.
    In(Box<Expr>, Vec<Literal>),
    /// `left and right`.
    And(Box<Expr>, Box<Expr>),
    /// `left or right`.
    Or(Box<Expr>, Box<Expr>),
    /// `not operand`.
    Not(Box<Expr>),
    /// A method call of the subset.
    Call(Method, Vec<Expr>),
    /// `collection/any(...)`.
    Any {
        /// The collection.
        collection: MemberPath,
        /// The lambda variable and predicate; `None` for `any()`.
        lambda: Option<(String, Box<Expr>)>,
    },
}

/// A path to a member: an optional lambda variable, then property, navigation property and
/// annotation names (annotations written with their `@`).
///
/// ```
/// use odata_url::MemberPath;
/// let area = MemberPath { variable: Some("r".into()), path: vec!["area".into()] };
/// assert_eq!(area.to_string(), "r/area");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberPath {
    /// The lambda variable the path starts at, if any.
    pub variable: Option<String>,
    /// The names after it.
    pub path: Vec<String>,
}

/// Comparison operators.
///
/// ```
/// use odata_url::{CompareOp, Expr, Literal};
/// let one = || Box::new(Expr::Literal(Literal::Int(1)));
/// assert_eq!(Expr::Compare(one(), CompareOp::Le, one()).to_string(), "1%20le%201");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `eq`.
    Eq,
    /// `ne`.
    Ne,
    /// `gt`.
    Gt,
    /// `ge`.
    Ge,
    /// `lt`.
    Lt,
    /// `le`.
    Le,
}

/// Method calls of the subset (T-3441 F15, F29).
///
/// ```
/// use odata_url::{Expr, Literal, Method};
/// let call = Expr::Call(Method::StartsWith, vec![Expr::Alias("n".into()), Expr::Literal(Literal::String("a".into()))]);
/// assert_eq!(call.to_string(), "startswith(@n,'a')");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `contains`.
    Contains,
    /// `startswith`.
    StartsWith,
    /// `endswith`.
    EndsWith,
    /// `geo.intersects`.
    GeoIntersects,
}

/// A primitive literal of the subset (T-3441 F26, F29).
///
/// ```
/// use odata_url::Literal;
/// assert_eq!(Literal::String("it's #1".into()).to_string(), "'it''s%20%231'");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// `null`.
    Null,
    /// `true`, `false`.
    Bool(bool),
    /// An integer in the `Int64` range.
    Int(i64),
    /// Any other number, as written (sign normalised to `+`/`-`): a fraction, an exponent, an
    /// integer beyond `Int64`, `NaN`, `INF`, `-INF`.
    Decimal(String),
    /// A string, percent-decoded, `''` read as `'`.
    String(String),
    /// `YYYY-MM-DD`.
    Date(String),
    /// A date-time with offset, colons normalised.
    DateTimeOffset(String),
    /// A time of day, colons normalised.
    TimeOfDay(String),
    /// `geography'SRID=n;Point(...)'` or `...;Polygon(...)'`.
    Geography(Geography),
}

/// A geography literal of the subset.
///
/// ```
/// use odata_url::{Geography, Literal, Position, Shape};
/// let point = Shape::Point(Position(vec!["7.1".into(), "50.7".into()]));
/// let at = Literal::Geography(Geography { srid: 4326, shape: point });
/// assert_eq!(at.to_string(), "geography'SRID=4326;Point(7.1%2050.7)'");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Geography {
    /// The SRID.
    pub srid: u32,
    /// The shape.
    pub shape: Shape,
}

/// Geography shapes of the subset.
///
/// ```
/// use odata_url::{Geography, Position, Shape};
/// let ring = vec![Position(vec!["0".into(), "0".into()]), Position(vec!["1".into(), "0".into()])];
/// let area = Geography { srid: 0, shape: Shape::Polygon(vec![ring]) };
/// assert_eq!(area.to_string(), "geography'SRID=0;Polygon((0%200,1%200))'");
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A point.
    Point(Position),
    /// A polygon: rings of positions, the first ring outer.
    Polygon(Vec<Vec<Position>>),
}

/// Coordinates: longitude, latitude, then optional altitude and measure, as written.
///
/// ```
/// use odata_url::Position;
/// assert_eq!(Position(vec!["1".into(), "2".into(), "3".into()]).to_string(), "1%202%203");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position(pub Vec<String>);
