//! `items` as a file (T-4465, EP-119): `f=parquet` or an `Accept` naming Parquet asks for the
//! whole answer of the request's own filters (bbox, datetime, CQL2 `filter`, the `ngsild.`
//! options) as one Apache Parquet file, `GeoParquet` 1.1 when the collection has a geometry.
//! The pages are read as `items` reads them, so the view narrows the file exactly as it
//! narrows the `GeoJSON`, and each page is written as it is read.

use axum::http::header::ACCEPT;
use axum::http::HeaderMap;

/// Whether the request asks `items` for Parquet, and its pairs without the `f=parquet` that
/// asked; without the `parquet` feature nothing asks for a file and the pairs are as sent.
pub(crate) fn asked(
    headers: &HeaderMap,
    pairs: Vec<(String, String)>,
) -> (Vec<(String, String)>, bool) {
    if !cfg!(feature = "parquet") {
        return (pairs, false);
    }
    let by_f = pairs.iter().any(|(n, v)| n == "f" && v == "parquet");
    let pairs = if by_f {
        pairs
            .into_iter()
            .filter(|(n, v)| !(n == "f" && v == "parquet"))
            .collect()
    } else {
        pairs
    };
    let by_accept = headers.get_all(ACCEPT).iter().any(|v| {
        v.to_str().is_ok_and(|v| {
            v.split(',')
                .map(|range| range.split(';').next().unwrap_or_default().trim())
                .any(|m| m.eq_ignore_ascii_case(PARQUET))
        })
    });
    (pairs, by_f || by_accept)
}

/// The media type of a Parquet file.
pub(crate) const PARQUET: &str = "application/vnd.apache.parquet";

#[cfg(feature = "parquet")]
mod file {
    use super::PARQUET;
    use crate::model::Collection;
    use crate::{items, problem, Here, Reply};
    use antares_export::parquet::{Error, Writer};
    use antares_export::{Column, Kind};
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;

    impl Here<'_> {
        /// The whole answer of `items` under `pairs` as one Parquet file: a column per
        /// property the view shows (those `properties` keeps), typed from the collection's
        /// schema, and the geometry as WKB.
        pub(crate) async fn parquet(&self, c: &Collection, pairs: &[(String, String)]) -> Reply {
            let mut p = match items::params(c, pairs) {
                Ok(p) => p,
                Err(refused) => return refused.reply(),
            };
            if p.crs != crate::crs::CRS84 {
                return problem(
                    StatusCode::BAD_REQUEST,
                    "a Parquet file is written in CRS84: leave crs out or name CRS84",
                );
            }
            p.limit = items::MAX_LIMIT;
            let columns = c
                .properties
                .iter()
                .filter(|prop| {
                    p.properties
                        .as_ref()
                        .is_none_or(|keep| keep.contains(&prop.iri))
                })
                .map(|prop| Column {
                    name: prop.name.clone(),
                    kind: Kind::of_schema(&prop.schema),
                })
                .collect();
            let mut writer = match Writer::new(columns, c.geometry.is_some(), self.max_bytes) {
                Ok(writer) => writer,
                Err(e) => return failed(&e),
            };
            let digest = items::digest(self.id, &c.id, pairs);
            loop {
                let page =
                    match items::page(self.view, self.caller, c, &p, digest, self.refs(p.profile))
                        .await
                    {
                        Ok(page) => page,
                        Err(refused) => return refused.reply(),
                    };
                if let Err(e) = writer.page(&page.features) {
                    return failed(&e);
                }
                match page.next {
                    // A token that does not move would read the same page again.
                    Some(next) if p.next.as_ref() != Some(&next) => p.next = Some(next),
                    _ => break,
                }
            }
            match writer.finish() {
                Ok(bytes) => (
                    StatusCode::OK,
                    [
                        (CONTENT_TYPE, PARQUET.to_owned()),
                        (
                            CONTENT_DISPOSITION,
                            format!("attachment; filename=\"{}.parquet\"", c.id),
                        ),
                    ],
                    bytes,
                )
                    .into_response(),
                Err(e) => failed(&e),
            }
        }
    }

    /// A file past the answer ceiling is refused whole, never sent cut.
    fn failed(e: &Error) -> Reply {
        match e {
            Error::TooLarge => problem(
                StatusCode::PAYLOAD_TOO_LARGE,
                antares_ops::bounds::ANSWER_TOO_LARGE,
            ),
            Error::Write(_) => problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the Parquet file could not be written",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn parquet_is_asked_by_f_or_by_accept_only_in_a_parquet_build() {
        let none = HeaderMap::new();
        let (kept, asked_for) = asked(&none, pairs(&[("f", "parquet"), ("limit", "5")]));
        assert_eq!(asked_for, cfg!(feature = "parquet"));
        if cfg!(feature = "parquet") {
            assert_eq!(
                kept,
                pairs(&[("limit", "5")]),
                "f=parquet is the encoding, no filter"
            );
        }
        let mut accept = HeaderMap::new();
        accept.insert(
            ACCEPT,
            HeaderValue::from_static("application/json;q=0.5, application/vnd.apache.parquet"),
        );
        assert_eq!(asked(&accept, Vec::new()).1, cfg!(feature = "parquet"));
        accept.insert(ACCEPT, HeaderValue::from_static("*/*"));
        assert!(!asked(&accept, Vec::new()).1, "anything is GeoJSON");
        let (kept, asked_for) = asked(&none, pairs(&[("f", "csv")]));
        assert!(!asked_for);
        assert_eq!(
            kept,
            pairs(&[("f", "csv")]),
            "another f stays to be refused"
        );
    }
}
