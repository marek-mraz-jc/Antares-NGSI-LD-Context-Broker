//! `primitiveLiteral` (ABNF section 7): the subset of T-3441 F26 and F29 as values, the other
//! literal kinds recognised and answered as not implemented.

use crate::cursor::Cursor;
use crate::{Feature, Geography, LimitKind, Limits, Literal, Position, Shape, UrlError};

/// A literal at the cursor, `None` (cursor unmoved) when none starts there. Once a literal has
/// certainly started (an opening quote, a `geography'` prefix) a malformed rest is an error.
///
/// The literal is read through a cursor that ends one byte past `max_literal_len`, so a longer
/// one is refused before its bytes are read or copied.
pub(crate) fn literal(c: &mut Cursor<'_>, limits: &Limits) -> Result<Option<Literal>, UrlError> {
    let start = c.pos();
    let end = start
        .saturating_add(limits.max_literal_len)
        .saturating_add(1);
    // No bound inside the URL: the rest is shorter, or the bound falls inside a non-ASCII
    // character, which no literal takes.
    let Some(mut bounded) = c.until(end) else {
        return any_literal(c);
    };
    let found = any_literal(&mut bounded);
    c.take(&bounded);
    // Any reading that ran into the bound is too long: the literal itself, or a longer kind
    // tried first (an overlong date-time is not read as the date it starts with). A failure
    // within a percent escape of the bound ran into it too: the bound cut the escape.
    let ran_into_bound = match found {
        Ok(None) => false,
        Ok(Some(_)) => bounded.reached_end(),
        Err(_) => bounded.reached_end_but(2),
    };
    if ran_into_bound {
        c.reset(start);
        return Err(c.limit(LimitKind::LiteralLength, limits.max_literal_len));
    }
    found
}

fn any_literal(c: &mut Cursor<'_>) -> Result<Option<Literal>, UrlError> {
    let start = c.pos();
    if keyword(c, "null", true) {
        return Ok(Some(Literal::Null));
    }
    for (word, value) in [("true", true), ("false", false)] {
        if keyword(c, word, false) {
            return Ok(Some(Literal::Bool(value)));
        }
    }
    if prefixed(c, "geography") {
        return geography(c).map(|g| Some(Literal::Geography(g)));
    }
    for prefix in ["geometry", "binary", "duration"] {
        if prefixed(c, prefix) {
            c.reset(start);
            return Err(c.not_implemented(Feature::Literal));
        }
    }
    if enum_prefix(c)? {
        c.reset(start);
        return Err(c.not_implemented(Feature::Literal));
    }
    if c.squote() {
        return string_body(c).map(|s| Some(Literal::String(s)));
    }
    if guid(c) {
        c.reset(start);
        return Err(c.not_implemented(Feature::Literal));
    }
    c.reset(start);
    if let Some(text) = date_time_offset(c) {
        return Ok(Some(Literal::DateTimeOffset(text)));
    }
    c.reset(start);
    if let Some(text) = date(c) {
        return Ok(Some(Literal::Date(text)));
    }
    c.reset(start);
    if let Some(text) = time_of_day(c) {
        return Ok(Some(Literal::TimeOfDay(text)));
    }
    c.reset(start);
    Ok(number(c))
}

fn continues_identifier(c: &Cursor<'_>) -> bool {
    c.peek()
        .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A word literal not followed by identifier characters (`nullable` is a name, not `null`).
fn keyword(c: &mut Cursor<'_>, word: &str, case_sensitive: bool) -> bool {
    let start = c.pos();
    let found = if case_sensitive {
        c.eat(word)
    } else {
        c.eat_ci(word)
    };
    if found && !continues_identifier(c) {
        return true;
    }
    c.reset(start);
    false
}

/// `prefix` directly followed by a quote, both consumed.
fn prefixed(c: &mut Cursor<'_>, prefix: &str) -> bool {
    let start = c.pos();
    if c.eat_ci(prefix) && c.squote() {
        return true;
    }
    c.reset(start);
    false
}

/// `qualifiedEnumTypeName SQUOTE`: a dotted name directly followed by a quote.
fn enum_prefix(c: &mut Cursor<'_>) -> Result<bool, UrlError> {
    let start = c.pos();
    let mut parts = 0_u8;
    while c.identifier()?.is_some() {
        parts = parts.saturating_add(1);
        if !c.eat(".") {
            break;
        }
    }
    let found = parts >= 2 && c.squote();
    c.reset(start);
    Ok(found)
}

/// The rest of `stringLiteral` after its opening quote, percent-decoded.
pub(crate) fn string_body(c: &mut Cursor<'_>) -> Result<String, UrlError> {
    let mut bytes = Vec::new();
    loop {
        if c.squote() {
            if c.squote() {
                bytes.push(b'\'');
                continue;
            }
            return String::from_utf8(bytes).map_err(|_| c.syntax("stringLiteral"));
        }
        // `%27` is a quote, taken by `squote` above.
        if let Some(byte) = pct_encoded(c) {
            bytes.push(byte);
        } else if let Some(byte) = c.eat_if(is_pchar_no_squote) {
            bytes.push(byte);
        } else {
            return Err(c.syntax("stringLiteral"));
        }
    }
}

/// `pchar-no-SQUOTE` without its percent-encoded part.
fn is_pchar_no_squote(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~!()*+,;$&=:@".contains(&b)
}

/// `"%" HEXDIG HEXDIG`, decoded.
pub(crate) fn pct_encoded(c: &mut Cursor<'_>) -> Option<u8> {
    let rest = c.rest().as_bytes();
    let (Some(b'%'), Some(&hi), Some(&lo)) = (rest.first(), rest.get(1), rest.get(2)) else {
        return None;
    };
    let value = hex(hi)?.checked_mul(16)?.checked_add(hex(lo)?)?;
    c.advance(3);
    Some(value)
}

fn hex(b: u8) -> Option<u8> {
    char::from(b)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

fn hexdigits(c: &mut Cursor<'_>, n: usize) -> bool {
    (0..n).all(|_| c.eat_if(|b| b.is_ascii_hexdigit()).is_some())
}

/// `guid`, consumed (the caller resets).
fn guid(c: &mut Cursor<'_>) -> bool {
    [8, 4, 4, 4]
        .into_iter()
        .all(|n| hexdigits(c, n) && c.eat("-"))
        && hexdigits(c, 12)
}

/// `n` digits exactly.
fn fixed_digits(c: &mut Cursor<'_>, n: usize) -> bool {
    (0..n).all(|_| c.eat_if(|b| b.is_ascii_digit()).is_some())
}

/// `date`: `year "-" month "-" day`.
fn date(c: &mut Cursor<'_>) -> Option<String> {
    let start = c.pos();
    c.eat("-");
    let year_ok = if c.eat("0") {
        fixed_digits(c, 3)
    } else {
        c.eat_if(|b| (b'1'..=b'9').contains(&b)).is_some() && fixed_digits(c, 3) && {
            c.digits();
            true
        }
    };
    let ok = year_ok && c.eat("-") && two(c, 1, 12) && c.eat("-") && two(c, 1, 31);
    ok.then(|| c.slice(start).to_owned())
}

/// Two digits forming a number from `low` to `high`, consumed as far as they match the ABNF
/// rule's leading digits.
fn two(c: &mut Cursor<'_>, low: u8, high: u8) -> bool {
    let (Some(tens), Some(ones)) = (c.peek(), c.peek_at(1)) else {
        return false;
    };
    if !tens.is_ascii_digit() {
        return false;
    }
    let tens_max = high / 10;
    let tens_value = tens.wrapping_sub(b'0');
    if tens_value > tens_max {
        return false;
    }
    c.advance(1);
    if !ones.is_ascii_digit() {
        return false;
    }
    let value = tens_value
        .saturating_mul(10)
        .saturating_add(ones.wrapping_sub(b'0'));
    if value < low || value > high {
        return false;
    }
    c.advance(1);
    true
}

/// `timeOfDayLiteral` (`COLON` may be `%3A`), colons normalised.
fn time_of_day(c: &mut Cursor<'_>) -> Option<String> {
    let colon = Cursor::colon;
    let mut text = String::new();
    let hour = c.rest().get(..2)?.to_owned();
    if !two(c, 0, 23) || !colon(c) {
        return None;
    }
    text.push_str(&hour);
    text.push(':');
    let minute = c.rest().get(..2)?.to_owned();
    if !two(c, 0, 59) {
        return None;
    }
    text.push_str(&minute);
    let before = c.pos();
    if colon(c) {
        let second = c.rest().get(..2).unwrap_or_default().to_owned();
        if !(two(c, 0, 59) || c.eat("60")) {
            c.reset(before);
            return Some(text);
        }
        text.push(':');
        text.push_str(&second);
        let dot = c.pos();
        if c.eat(".") {
            let fraction = c.digits();
            if fraction.is_empty() || fraction.len() > 12 {
                c.reset(dot);
                return Some(text);
            }
            text.push('.');
            text.push_str(fraction);
        }
    }
    Some(text)
}

/// `dateTimeOffsetLiteral`: `date "T" timeOfDayLiteral ( "Z" / SIGN hour COLON minute )`.
fn date_time_offset(c: &mut Cursor<'_>) -> Option<String> {
    let mut text = date(c)?;
    if !c.eat_ci("T") {
        return None;
    }
    text.push('T');
    text.push_str(&time_of_day(c)?);
    if c.eat_ci("Z") {
        text.push('Z');
        return Some(text);
    }
    text.push(c.sign()?);
    let hour = c.rest().get(..2)?.to_owned();
    if !two(c, 0, 23) || !c.colon() {
        return None;
    }
    let minute = c.rest().get(..2)?.to_owned();
    if !two(c, 0, 59) {
        return None;
    }
    text.push_str(&hour);
    text.push(':');
    text.push_str(&minute);
    Some(text)
}

/// `decimalLiteral` (`SIGN` may be `%2B`): an `Int64` as [`Literal::Int`], everything else as
/// [`Literal::Decimal`] text.
pub(crate) fn number(c: &mut Cursor<'_>) -> Option<Literal> {
    let (text, integer) = number_text(c, true)?;
    if integer {
        if let Ok(value) = text.parse::<i64>() {
            return Some(Literal::Int(value));
        }
    }
    Some(Literal::Decimal(text))
}

/// `decimalLiteral` (`in_url`) or `decimalValue` as written, and whether it is an integer.
fn number_text(c: &mut Cursor<'_>, in_url: bool) -> Option<(String, bool)> {
    let start = c.pos();
    let sign = if in_url {
        c.sign()
    } else if c.eat("+") {
        Some('+')
    } else if c.eat("-") {
        Some('-')
    } else {
        None
    };
    let digits = c.digits();
    if digits.is_empty() {
        c.reset(start);
        for word in ["NaN", "-INF", "INF"] {
            if keyword(c, word, true) {
                return Some((word.to_owned(), false));
            }
        }
        return None;
    }
    let mut text: String = sign.into_iter().collect();
    text.push_str(digits);
    let mut integer = true;
    let dot = c.pos();
    if c.eat(".") {
        let fraction = c.digits();
        if fraction.is_empty() {
            c.reset(dot);
        } else {
            integer = false;
            text.push('.');
            text.push_str(fraction);
        }
    }
    let exponent = c.pos();
    if c.eat_ci("e") {
        let exp_sign = if in_url {
            c.sign()
        } else {
            c.eat("-")
                .then_some('-')
                .or_else(|| c.eat("+").then_some('+'))
        };
        let exp_digits = c.digits();
        if exp_digits.is_empty() {
            c.reset(exponent);
        } else {
            integer = false;
            text.push('e');
            text.extend(exp_sign);
            text.push_str(exp_digits);
        }
    }
    Some((text, integer))
}

/// The rest of a `geography'...'` literal after its quote: a point or a polygon of the
/// subset, any other shape not implemented.
fn geography(c: &mut Cursor<'_>) -> Result<Geography, UrlError> {
    if !c.eat_ci("SRID") || !c.eat("=") {
        return Err(c.syntax("sridLiteral"));
    }
    let digits = c.digits();
    let srid = Some(digits)
        .filter(|d| (1..=5).contains(&d.len()))
        .and_then(|d| d.parse().ok())
        .ok_or_else(|| c.syntax("sridLiteral"))?;
    if !c.semi() {
        return Err(c.syntax("sridLiteral"));
    }
    let shape = if c.eat_ci("Point") {
        Shape::Point(point_data(c)?)
    } else if c.eat_ci("Polygon") {
        Shape::Polygon(polygon_data(c)?)
    } else if [
        "LineString",
        "MultiLineString",
        "MultiPoint",
        "MultiPolygon",
        "GeometryCollection",
    ]
    .into_iter()
    .any(|shape| {
        c.rest()
            .get(..shape.len())
            .is_some_and(|h| h.eq_ignore_ascii_case(shape))
    }) {
        return Err(c.not_implemented(Feature::Literal));
    } else {
        return Err(c.syntax("geoLiteral"));
    };
    if !c.squote() {
        return Err(c.syntax("geographyPrefix SQUOTE"));
    }
    Ok(Geography { srid, shape })
}

fn point_data(c: &mut Cursor<'_>) -> Result<Position, UrlError> {
    if !c.open() {
        return Err(c.syntax("pointData"));
    }
    let position = position(c)?;
    if !c.close() {
        return Err(c.syntax("pointData"));
    }
    Ok(position)
}

fn polygon_data(c: &mut Cursor<'_>) -> Result<Vec<Vec<Position>>, UrlError> {
    if !c.open() {
        return Err(c.syntax("polygonData"));
    }
    let mut rings = Vec::new();
    loop {
        if !c.open() {
            return Err(c.syntax("ringLiteral"));
        }
        let mut ring = vec![position(c)?];
        while c.comma() {
            ring.push(position(c)?);
        }
        if !c.close() {
            return Err(c.syntax("ringLiteral"));
        }
        rings.push(ring);
        if !c.comma() {
            break;
        }
    }
    if !c.close() {
        return Err(c.syntax("polygonData"));
    }
    Ok(rings)
}

/// `positionLiteral`: two to four `doubleValue`s separated by `SP` (or `%20`).
fn position(c: &mut Cursor<'_>) -> Result<Position, UrlError> {
    let mut values = vec![double_value(c)?];
    for required in [true, false, false] {
        let before = c.pos();
        if !(c.eat(" ") || c.eat("%20")) {
            if required {
                return Err(c.syntax("positionLiteral"));
            }
            break;
        }
        match number_text(c, false) {
            Some((value, _)) => values.push(value),
            None if required => return Err(c.syntax("doubleValue")),
            None => {
                c.reset(before);
                break;
            }
        }
    }
    Ok(Position(values))
}

fn double_value(c: &mut Cursor<'_>) -> Result<String, UrlError> {
    number_text(c, false)
        .map(|(value, _)| value)
        .ok_or_else(|| c.syntax("doubleValue"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(input: &str) -> Result<Option<Literal>, UrlError> {
        let mut c = Cursor::new(input);
        let found = literal(&mut c, &Limits::strict())?;
        assert_eq!(found.as_ref().map_or("", |_| c.rest()), "", "{input}");
        Ok(found)
    }

    /// The literal at the start of `input` and what is left of it.
    fn first(input: &str) -> (Option<Literal>, String) {
        let mut c = Cursor::new(input);
        let found = literal(&mut c, &Limits::strict());
        assert!(found.is_ok(), "{input}: {found:?}");
        (found.unwrap_or_default(), c.rest().to_owned())
    }

    #[test]
    fn partial_matches_stop_where_the_rule_does() {
        let time = |t: &str| Some(Literal::TimeOfDay(t.into()));
        let date = |t: &str| Some(Literal::Date(t.into()));
        for (input, literal, rest) in [
            ("2024-13-01", Some(Literal::Int(2024)), "-13-01"),
            ("2024-00-01", Some(Literal::Int(2024)), "-00-01"),
            ("2024-1", Some(Literal::Int(2024)), "-1"),
            ("10:60", Some(Literal::Int(10)), ":60"),
            ("10:00:61", time("10:00"), ":61"),
            ("10:00:60", time("10:00:60"), ""),
            ("10:00:00.", time("10:00:00"), "."),
            ("10:00:00.1234567890123", time("10:00:00"), ".1234567890123"),
            ("2024-01-01T10:00+25:00", date("2024-01-01"), "T10:00+25:00"),
            ("2024-01-01T10:00+01x00", date("2024-01-01"), "T10:00+01x00"),
            ("2024-01-01T10:00+01:60", date("2024-01-01"), "T10:00+01:60"),
            ("2024-01-01X", date("2024-01-01"), "X"),
            ("2024-01x", Some(Literal::Int(2024)), "-01x"),
            ("geography", None, "geography"),
        ] {
            let found = first(input);
            assert_eq!(found, (literal, rest.to_owned()), "{input}");
        }
        let unclosed = read("'a%4'");
        assert!(
            matches!(
                unclosed,
                Err(UrlError {
                    kind: crate::UrlErrorKind::Syntax { .. },
                    offset: 2
                })
            ),
            "{unclosed:?}"
        );
    }

    #[test]
    fn geography_errors() {
        for (input, offset) in [
            ("geography'Point(1 2)'", 10),
            ("geography'SRID=x;Point(1 2)'", 15),
            ("geography'SRID=0;Polygon(1 2)'", 25),
            ("geography'SRID=0;Polygon 1'", 24),
            ("geography'SRID'", 14),
            ("geography'SRID=0;Polygon((0 0,1 1'", 33),
        ] {
            let e = read(input);
            assert!(
                matches!(
                    e,
                    Err(UrlError {
                        kind: crate::UrlErrorKind::Syntax { .. },
                        ..
                    })
                ),
                "{input}: {e:?}"
            );
            assert_eq!(e.err().map(|e| e.offset), Some(offset), "{input}");
        }
        let signed = ok("geography'SRID=0;Point(+1 -2)'");
        let point = Shape::Point(Position(vec!["+1".into(), "-2".into()]));
        assert_eq!(
            signed,
            Literal::Geography(Geography {
                srid: 0,
                shape: point
            })
        );
    }

    fn ok(input: &str) -> Literal {
        read(input).unwrap().unwrap()
    }

    #[test]
    fn words() {
        assert_eq!(ok("null"), Literal::Null);
        assert_eq!(ok("true"), Literal::Bool(true));
        assert_eq!(ok("FALSE"), Literal::Bool(false));
        assert_eq!(read("nullable").unwrap(), None);
        assert_eq!(read("NULL").unwrap(), None);
        assert_eq!(read("trueish").unwrap(), None);
    }

    #[test]
    fn numbers() {
        assert_eq!(ok("42"), Literal::Int(42));
        assert_eq!(ok("-42"), Literal::Int(-42));
        assert_eq!(ok("%2B42"), Literal::Int(42));
        assert_eq!(ok("1.5"), Literal::Decimal("1.5".into()));
        assert_eq!(ok("-0.314e1"), Literal::Decimal("-0.314e1".into()));
        assert_eq!(ok("1E-3"), Literal::Decimal("1e-3".into()));
        assert_eq!(ok("1e%2B3"), Literal::Decimal("1e+3".into()));
        assert_eq!(ok("NaN"), Literal::Decimal("NaN".into()));
        assert_eq!(ok("-INF"), Literal::Decimal("-INF".into()));
        assert_eq!(ok("INF"), Literal::Decimal("INF".into()));
        assert_eq!(
            ok("99999999999999999999"),
            Literal::Decimal("99999999999999999999".into())
        );
        assert_eq!(read("INFO").unwrap(), None);
        // A trailing dot or a bare exponent is not part of the number.
        let mut c = Cursor::new("42.");
        assert_eq!(number(&mut c), Some(Literal::Int(42)));
        assert_eq!(c.rest(), ".");
        let mut c = Cursor::new("4e");
        assert_eq!(number(&mut c), Some(Literal::Int(4)));
        assert_eq!(c.rest(), "e");
    }

    #[test]
    fn strings() {
        assert_eq!(ok("'O''Neil'"), Literal::String("O'Neil".into()));
        assert_eq!(ok("%27O%27%27Neil%27"), Literal::String("O'Neil".into()));
        assert_eq!(ok("'a%20b%C3%A9'"), Literal::String("a bé".into()));
        assert_eq!(ok("''"), Literal::String(String::new()));
        let unterminated = read("'abc").unwrap_err();
        assert_eq!(unterminated.offset, 4);
        let slash = read("'a/b'").unwrap_err();
        assert_eq!(slash.offset, 2);
        let invalid_utf8 = read("'%FF'").unwrap_err();
        assert_eq!(invalid_utf8.offset, 5);
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(ok("2012-09-03"), Literal::Date("2012-09-03".into()));
        assert_eq!(ok("-0001-01-01"), Literal::Date("-0001-01-01".into()));
        assert_eq!(ok("12012-12-31"), Literal::Date("12012-12-31".into()));
        assert_eq!(
            ok("2012-09-03T13:52:00.123456789012Z"),
            Literal::DateTimeOffset("2012-09-03T13:52:00.123456789012Z".into())
        );
        assert_eq!(
            ok("2012-09-03t13%3A52-03%3A00"),
            Literal::DateTimeOffset("2012-09-03T13:52-03:00".into())
        );
        assert_eq!(ok("13:52"), Literal::TimeOfDay("13:52".into()));
        assert_eq!(ok("23:59:60"), Literal::TimeOfDay("23:59:60".into()));
        assert_eq!(
            ok("07:59:59.999"),
            Literal::TimeOfDay("07:59:59.999".into())
        );
        // Not a date: the digits are a number.
        assert_eq!(ok("2012"), Literal::Int(2012));
        let mut c = Cursor::new("2012-13-01");
        assert_eq!(
            literal(&mut c, &Limits::strict()).unwrap(),
            Some(Literal::Int(2012))
        );
        let mut c = Cursor::new("24:00");
        assert_eq!(
            literal(&mut c, &Limits::strict()).unwrap(),
            Some(Literal::Int(24))
        );
    }

    #[test]
    fn geography() {
        assert_eq!(
            ok("geography'SRID=4326;Point(-1.5 2E1)'"),
            Literal::Geography(Geography {
                srid: 4326,
                shape: Shape::Point(Position(vec!["-1.5".into(), "2e1".into()])),
            })
        );
        assert_eq!(
            ok("geography'SRID=0;Polygon((0 0,1%201,0 1 2 3,0 0),(5 5,6 6))'"),
            Literal::Geography(Geography {
                srid: 0,
                shape: Shape::Polygon(vec![
                    vec![
                        Position(vec!["0".into(), "0".into()]),
                        Position(vec!["1".into(), "1".into()]),
                        Position(vec!["0".into(), "1".into(), "2".into(), "3".into()]),
                        Position(vec!["0".into(), "0".into()]),
                    ],
                    vec![
                        Position(vec!["5".into(), "5".into()]),
                        Position(vec!["6".into(), "6".into()]),
                    ],
                ]),
            })
        );
        for bad in [
            "geography'Point(1 2)'",
            "geography'SRID=123456;Point(1 2)'",
            "geography'SRID=1,Point(1 2)'",
            "geography'SRID=1;Spot(1 2)'",
            "geography'SRID=1;Point 1 2)'",
            "geography'SRID=1;Point(1 2'",
            "geography'SRID=1;Point(1)'",
            "geography'SRID=1;Point(1 x)'",
            "geography'SRID=1;Point(1 2)",
            "geography'SRID=1;Polygon(0 0)'",
            "geography'SRID=1;Polygon((0 0)'",
            "geography'SRID=1;Polygon((0 0),'",
            "geography'SRID=1;Polygon((x 0))'",
        ] {
            assert!(
                matches!(
                    read(bad).unwrap_err().kind,
                    crate::UrlErrorKind::Syntax { .. }
                ),
                "{bad}"
            );
        }
        // A third coordinate that is not a number ends the position.
        let mut c = Cursor::new("1 2 x");
        assert_eq!(
            position(&mut c).unwrap(),
            Position(vec!["1".into(), "2".into()])
        );
        assert_eq!(c.rest(), " x");
    }

    #[test]
    fn kinds_outside_the_subset() {
        for other in [
            "geography'SRID=0;LineString(1 1,2 2)'",
            "geometry'SRID=0;Point(1 1)'",
            "binary'T0RhdGE'",
            "duration'P12DT23H59M59.999999999999S'",
            "Namespace.Pattern'Yellow'",
            "01234567-89ab-cdef-0123-456789abcdef",
        ] {
            assert_eq!(
                read(other).unwrap_err().kind,
                crate::UrlErrorKind::NotImplemented(Feature::Literal),
                "{other}"
            );
        }
        // A dotted name without a quote is a path, not an enum.
        assert_eq!(read("Namespace.Pattern").unwrap(), None);
        assert_eq!(read("Name").unwrap(), None);
    }

    #[test]
    fn a_literal_is_bounded() {
        let limits = Limits {
            max_literal_len: 4,
            ..Limits::strict()
        };
        let mut c = Cursor::new("'abc'");
        assert_eq!(
            literal(&mut c, &limits).unwrap_err().kind,
            crate::UrlErrorKind::LimitExceeded {
                limit: LimitKind::LiteralLength,
                max: 4
            }
        );
        let mut c = Cursor::new("'ab'");
        assert!(literal(&mut c, &limits).unwrap().is_some());
    }
}
