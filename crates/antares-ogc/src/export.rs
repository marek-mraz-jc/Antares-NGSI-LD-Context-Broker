//! `items` as a file (T-4465, EP-119): `f=parquet` or an `Accept` naming Parquet asks for the
//! whole answer of the request's own filters (bbox, datetime, CQL2 `filter`, the `ngsild.`
//! options) as one Apache Parquet file, `GeoParquet` 1.1 when the collection has a geometry.
//! The pages are read as `items` reads them, so the view narrows the file exactly as it
//! narrows the `GeoJSON`, and each page is written as it is read.

use axum::http::HeaderMap;

/// The media type of a Parquet file.
#[cfg(feature = "parquet")]
pub(crate) const PARQUET: &str = "application/vnd.apache.parquet";

/// The file encodings `items` writes in this build, by their `f` value.
#[cfg(feature = "parquet")]
pub(crate) const ENCODINGS: &[&str] = &["parquet"];
/// Without the `parquet` feature `items` writes no file: an `f=parquet` stays among the pairs
/// and is refused like any other unknown parameter.
#[cfg(not(feature = "parquet"))]
pub(crate) const ENCODINGS: &[&str] = &[];

/// The media types of those encodings, as an `Accept` names them.
#[cfg(feature = "parquet")]
const MEDIA_TYPES: &[&str] = &[PARQUET];
#[cfg(not(feature = "parquet"))]
const MEDIA_TYPES: &[&str] = &[];

/// Whether the request asks `items` for a file, and its pairs without the `f` that asked.
pub(crate) fn asked(headers: &HeaderMap, pairs: Vec<(String, String)>) -> crate::Asked {
    let encoding = |(n, v): &(String, String)| n == "f" && ENCODINGS.contains(&v.as_str());
    let by_f = pairs.iter().any(encoding);
    let pairs = pairs.into_iter().filter(|pair| !encoding(pair)).collect();
    let by_accept = headers
        .get_all(axum::http::header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|range| range.split(';').next().unwrap_or_default().trim())
        .any(|m| MEDIA_TYPES.iter().any(|t| m.eq_ignore_ascii_case(t)));
    crate::Asked {
        pairs,
        file: by_f || by_accept,
    }
}

#[cfg(feature = "parquet")]
mod file {
    use super::PARQUET;
    use crate::model::Collection;
    use crate::{items, Here, Problem, Reply};
    use antares_export::parquet::Writer;
    use antares_export::{Column, Error, Kind, Sink};
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;

    /// A file past the answer ceiling is refused whole, never sent cut.
    impl From<Error> for Problem {
        fn from(e: Error) -> Self {
            match e {
                Error::TooLarge => Problem(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    antares_ops::bounds::ANSWER_TOO_LARGE,
                ),
                Error::Write(_) => Problem(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the Parquet file could not be written",
                ),
            }
        }
    }

    impl Here<'_> {
        /// The whole answer of `items` under `pairs` as one Parquet file: a column per
        /// property the view shows (those `properties` keeps), typed from the collection's
        /// schema, and the geometry as WKB.
        pub(crate) async fn parquet(&self, c: &Collection, pairs: &[(String, String)]) -> Reply {
            match self.file(c, pairs).await {
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
                Err(refused) => refused.reply(),
            }
        }

        async fn file(
            &self,
            c: &Collection,
            pairs: &[(String, String)],
        ) -> Result<Vec<u8>, Problem> {
            let mut p = items::params(c, pairs)?;
            if p.crs != crate::crs::CRS84 {
                return Err(Problem(
                    StatusCode::BAD_REQUEST,
                    "a Parquet file is written in CRS84: leave crs out or name CRS84",
                ));
            }
            p.limit = items::MAX_LIMIT;
            let keep = p.properties.clone();
            let columns = c
                .properties
                .iter()
                .filter(|prop| keep.as_ref().is_none_or(|keep| keep.contains(&prop.iri)))
                .map(|prop| Column {
                    name: prop.name.clone(),
                    kind: Kind::of_schema(&prop.schema),
                })
                .collect();
            let mut writer = Writer::new(columns, c.geometry.is_some(), self.max_bytes)?;
            let digest = items::digest(self.id, &c.id, pairs);
            loop {
                let refs = self.refs(p.profile);
                let page = items::page(self.view, self.caller, c, &p, digest, refs).await?;
                writer.page(&page.features)?;
                // A token that does not move would read the same page again.
                let next = page.next.filter(|next| p.next.as_ref() != Some(next));
                if next.is_none() {
                    break;
                }
                p.next = next;
            }
            Ok(Box::new(writer).finish(&[])?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn parquet_is_asked_by_f_or_by_accept() {
        use axum::http::HeaderValue;
        let none = HeaderMap::new();
        let asked_for = asked(&none, pairs(&[("f", "parquet"), ("limit", "5")]));
        assert!(asked_for.file);
        assert_eq!(
            asked_for.pairs,
            pairs(&[("limit", "5")]),
            "f=parquet is the encoding, no filter"
        );
        let mut accept = HeaderMap::new();
        accept.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("application/json;q=0.5, application/vnd.apache.parquet"),
        );
        assert!(asked(&accept, Vec::new()).file);
        accept.insert(axum::http::header::ACCEPT, HeaderValue::from_static("*/*"));
        assert!(!asked(&accept, Vec::new()).file, "anything is GeoJSON");
        let asked_for = asked(&none, pairs(&[("f", "csv")]));
        assert!(!asked_for.file);
        assert_eq!(
            asked_for.pairs,
            pairs(&[("f", "csv")]),
            "another f stays to be refused"
        );
    }

    #[cfg(feature = "parquet")]
    #[test]
    fn a_file_the_writer_refuses_is_a_500_naming_nothing() {
        let refused = crate::Problem::from(antares_export::Error::Write("x".into()));
        assert_eq!(
            refused,
            crate::Problem(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "the Parquet file could not be written"
            )
        );
    }

    #[test]
    fn f_asks_for_a_file_only_in_a_build_that_writes_it() {
        let asked_for = asked(&HeaderMap::new(), pairs(&[("f", "parquet")]));
        assert_eq!(asked_for.file, ENCODINGS.contains(&"parquet"));
        assert_eq!(asked_for.pairs.is_empty(), asked_for.file);
    }
}
