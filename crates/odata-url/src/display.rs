//! The canonical form of an [`ODataUri`]: what [`parse`](crate::parse) reads back to the same
//! tree. Spaces are written `%20`, system options in a fixed order with their `$`, and
//! parentheses only where the precedence needs them.

use std::fmt::{self, Display, Formatter, Write};

use crate::{
    CompareOp, ExpandItem, ExpandKind, ExpandPath, Expr, Format, Geography, Key, KeyValue, Levels,
    Literal, MemberPath, Method, ODataUri, OrderBy, Position, QueryOptions, Segment, SelectItem,
    Shape,
};

impl Display for ODataUri {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ODataUri::ServiceDocument => Ok(()),
            ODataUri::Metadata { options } => write!(f, "$metadata{}", Query(options)),
            ODataUri::Batch { options } => write!(f, "$batch{}", Query(options)),
            ODataUri::Entity { options } => write!(f, "$entity{}", Query(options)),
            ODataUri::Resource { path, options } => {
                for (i, segment) in path.iter().enumerate() {
                    if i > 0 && !matches!(segment, Segment::Key(_)) {
                        f.write_char('/')?;
                    }
                    write!(f, "{segment}")?;
                }
                write!(f, "{}", Query(options))
            }
        }
    }
}

impl Display for Segment {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Segment::EntitySet(name) | Segment::Property(name) | Segment::Navigation(name) => {
                f.write_str(name)
            }
            Segment::Key(Key::Single(value)) => write!(f, "({value})"),
            Segment::Key(Key::Compound(pairs)) => {
                f.write_char('(')?;
                for (i, (name, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "{name}={value}")?;
                }
                f.write_char(')')
            }
            Segment::Count => f.write_str("$count"),
            Segment::Ref => f.write_str("$ref"),
            Segment::Value => f.write_str("$value"),
            Segment::Query => f.write_str("$query"),
        }
    }
}

impl Display for KeyValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            KeyValue::Literal(literal) => write!(f, "{literal}"),
            KeyValue::Alias(name) => write!(f, "@{name}"),
        }
    }
}

/// `?` and the options, nothing when there are none.
struct Query<'o>(&'o QueryOptions);

impl Display for Query<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let o = self.0;
        let mut items: Vec<String> = Vec::new();
        if let Some(filter) = &o.filter {
            items.push(format!("$filter={filter}"));
        }
        if let Some(select) = &o.select {
            items.push(format!("$select={}", List(select)));
        }
        if let Some(expand) = &o.expand {
            items.push(format!("$expand={}", List(expand)));
        }
        if let Some(orderby) = &o.orderby {
            items.push(format!("$orderby={}", List(orderby)));
        }
        if let Some(top) = o.top {
            items.push(format!("$top={top}"));
        }
        if let Some(skip) = o.skip {
            items.push(format!("$skip={skip}"));
        }
        if let Some(count) = o.count {
            items.push(format!("$count={count}"));
        }
        if let Some(format) = &o.format {
            items.push(format!("$format={format}"));
        }
        if let Some(token) = &o.skiptoken {
            items.push(format!("$skiptoken={token}"));
        }
        if let Some(version) = &o.schemaversion {
            items.push(format!("$schemaversion={version}"));
        }
        if let Some(id) = &o.id {
            items.push(format!("$id={id}"));
        }
        for (name, value) in &o.aliases {
            items.push(format!("@{name}={value}"));
        }
        for (name, value) in &o.custom {
            match value {
                Some(value) => items.push(format!("{name}={value}")),
                None => items.push(name.clone()),
            }
        }
        if items.is_empty() {
            return Ok(());
        }
        write!(f, "?{}", items.join("&"))
    }
}

/// Items separated by `,`.
struct List<'l, T>(&'l [T]);

impl<T: Display> Display for List<'_, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, item) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_char(',')?;
            }
            write!(f, "{item}")?;
        }
        Ok(())
    }
}

impl Display for SelectItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Star => f.write_char('*'),
            SelectItem::Path(path) => f.write_str(&path.join("/")),
            SelectItem::Annotation(name) => f.write_str(&annotation(name)),
        }
    }
}

impl Display for ExpandItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self.path {
            ExpandPath::Star => f.write_char('*')?,
            ExpandPath::Path(path) => f.write_str(&path.join("/"))?,
        }
        match &self.kind {
            ExpandKind::Ref => f.write_str("/$ref"),
            ExpandKind::Entities {
                select,
                expand,
                levels,
            } => {
                let mut nested = Vec::new();
                if let Some(select) = select {
                    nested.push(format!("$select={}", List(select)));
                }
                if let Some(expand) = expand {
                    nested.push(format!("$expand={}", List(expand)));
                }
                if let Some(levels) = levels {
                    nested.push(format!("$levels={levels}"));
                }
                if nested.is_empty() {
                    return Ok(());
                }
                write!(f, "({})", nested.join(";"))
            }
        }
    }
}

impl Display for Levels {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Levels::Count(n) => write!(f, "{n}"),
            Levels::Max => f.write_str("max"),
        }
    }
}

impl Display for OrderBy {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.expr)?;
        if self.descending {
            f.write_str("%20desc")?;
        }
        Ok(())
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Format::Json => f.write_str("json"),
            Format::Xml => f.write_str("xml"),
            Format::Atom => f.write_str("atom"),
            Format::MediaType(text) => f.write_str(text),
        }
    }
}

/// Binding strength, URL Conventions 5.1.1.16: a child weaker than its place is parenthesised.
fn strength(expr: &Expr) -> u8 {
    match expr {
        Expr::Or(..) => 1,
        Expr::And(..) => 2,
        Expr::Compare(_, CompareOp::Eq | CompareOp::Ne, _) | Expr::In(..) => 3,
        Expr::Compare(..) => 4,
        Expr::Not(_) => 5,
        _ => 6,
    }
}

/// `expr`, parenthesised when it binds weaker than `at_least`.
struct At<'e>(&'e Expr, u8);

impl Display for At<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if strength(self.0) < self.1 {
            write!(f, "({})", self.0)
        } else {
            write!(f, "{}", self.0)
        }
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let s = strength(self);
        let next = s.saturating_add(1);
        match self {
            Expr::Literal(literal) => write!(f, "{literal}"),
            Expr::Alias(name) => write!(f, "@{name}"),
            Expr::Member(path) => write!(f, "{path}"),
            Expr::Compare(left, op, right) => {
                let word = match op {
                    CompareOp::Eq => "eq",
                    CompareOp::Ne => "ne",
                    CompareOp::Gt => "gt",
                    CompareOp::Ge => "ge",
                    CompareOp::Lt => "lt",
                    CompareOp::Le => "le",
                };
                write!(f, "{}%20{word}%20{}", At(left, s), At(right, next))
            }
            Expr::In(left, list) => write!(f, "{}%20in%20({})", At(left, s), List(list)),
            Expr::And(left, right) => write!(f, "{}%20and%20{}", At(left, s), At(right, next)),
            Expr::Or(left, right) => write!(f, "{}%20or%20{}", At(left, s), At(right, next)),
            Expr::Not(operand) => write!(f, "not%20{}", At(operand, s)),
            Expr::Call(method, args) => {
                let name = match method {
                    Method::Contains => "contains",
                    Method::StartsWith => "startswith",
                    Method::EndsWith => "endswith",
                    Method::GeoIntersects => "geo.intersects",
                };
                write!(f, "{name}({})", List(args))
            }
            Expr::Any { collection, lambda } => match lambda {
                None => write!(f, "{collection}/any()"),
                Some((variable, body)) => write!(f, "{collection}/any({variable}:{body})"),
            },
        }
    }
}

impl Display for MemberPath {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let parts = self
            .variable
            .iter()
            .chain(&self.path)
            .map(|p| annotation(p));
        for (i, part) in parts.enumerate() {
            if i > 0 {
                f.write_char('/')?;
            }
            f.write_str(&part)?;
        }
        Ok(())
    }
}

/// An annotation name with its `#` written `%23` (a raw `#` ends the URL); any other name as is.
fn annotation(name: &str) -> String {
    name.replace('#', "%23")
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Null => f.write_str("null"),
            Literal::Bool(value) => write!(f, "{value}"),
            Literal::Int(value) => write!(f, "{value}"),
            Literal::Decimal(text)
            | Literal::Date(text)
            | Literal::DateTimeOffset(text)
            | Literal::TimeOfDay(text) => f.write_str(&text.replace('+', "%2B")),
            Literal::String(text) => {
                f.write_char('\'')?;
                for byte in text.bytes() {
                    match byte {
                        b'\'' => f.write_str("''")?,
                        b if b.is_ascii_alphanumeric() || b"-._~!()*,;$&=:@".contains(&b) => {
                            f.write_char(char::from(b))?;
                        }
                        b => write!(f, "%{b:02X}")?,
                    }
                }
                f.write_char('\'')
            }
            Literal::Geography(geography) => write!(f, "{geography}"),
        }
    }
}

impl Display for Geography {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "geography'SRID={};", self.srid)?;
        match &self.shape {
            Shape::Point(position) => write!(f, "Point({position})")?,
            Shape::Polygon(rings) => {
                f.write_str("Polygon(")?;
                for (i, ring) in rings.iter().enumerate() {
                    if i > 0 {
                        f.write_char(',')?;
                    }
                    write!(f, "({})", List(ring))?;
                }
                f.write_char(')')?;
            }
        }
        f.write_char('\'')
    }
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("%20"))
    }
}
