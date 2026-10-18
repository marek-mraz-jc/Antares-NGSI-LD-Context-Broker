//! Files (T-4465, T-4477, EP-119, EP-08, EP-41). `items` as one file: `f=parquet` (Apache
//! Parquet, `GeoParquet` 1.1 with a geometry), `f=csv` or `f=xlsx`, or an `Accept` naming one,
//! holds the whole answer of the request's own filters (bbox, datetime, CQL2 `filter`, the
//! `ngsild.` options). `/collections` as one file: `f=xlsx` (a sheet per collection) or `f=zip`
//! (a CSV per collection, and its Parquet in a `parquet` build), filtered by `bbox` and
//! `datetime`, narrowed by `collections`. A file holds one type per table; a CSV or a Parquet of
//! several collections is refused, naming the two ways to get them. The pages are read as
//! `items` reads them, so the view narrows a file exactly as it narrows the `GeoJSON`, and each
//! page is written as it is read, under the answer ceiling.

use axum::http::HeaderMap;

/// The media type of a Parquet file.
#[cfg(feature = "parquet")]
pub(crate) const PARQUET: &str = "application/vnd.apache.parquet";

/// The files `items` writes in this build: the `f` value, the media type (its essence is what
/// an `Accept` names), and the writer.
#[cfg(any(feature = "parquet", feature = "tabular"))]
pub(crate) const ITEMS: &[(&str, &str, file::Make)] = &[
    #[cfg(feature = "parquet")]
    ("parquet", PARQUET, file::parquet),
    #[cfg(feature = "tabular")]
    ("csv", antares_export::csv::MEDIA_TYPE, file::csv),
    #[cfg(feature = "tabular")]
    ("xlsx", antares_export::xlsx::MEDIA_TYPE, file::xlsx),
];
/// Without a file feature `items` writes no file: an `f=` stays among the pairs and is refused
/// like any other unknown parameter.
#[cfg(not(any(feature = "parquet", feature = "tabular")))]
pub(crate) const ITEMS: &[(&str, &str, ())] = &[];

/// The files `/collections` writes of every collection the view shows.
#[cfg(feature = "tabular")]
pub(crate) const BUNDLES: &[(&str, &str)] = &[
    ("xlsx", antares_export::xlsx::MEDIA_TYPE),
    ("zip", antares_export::bundle::MEDIA_TYPE),
];
#[cfg(not(feature = "tabular"))]
pub(crate) const BUNDLES: &[(&str, &str)] = &[];

/// The `f` values of a file of one collection, refused on `/collections`.
#[cfg(feature = "tabular")]
pub(crate) const ONE_TYPE: &[&str] = &["csv", "parquet"];
#[cfg(not(feature = "tabular"))]
pub(crate) const ONE_TYPE: &[&str] = &[];

/// Whether the request asks for one of `offered` (`f` value, media type) by `f` or by its
/// `Accept`, and its pairs without the `f` that asked.
pub(crate) fn asked(
    headers: &HeaderMap,
    pairs: Vec<(String, String)>,
    offered: &[(&'static str, &'static str)],
) -> crate::Asked {
    let by_f = offered
        .iter()
        .find(|(f, _)| pairs.iter().any(|(n, v)| n == "f" && v == f));
    let pairs = pairs
        .into_iter()
        .filter(|(n, v)| !(n == "f" && by_f.is_some_and(|(f, _)| v == f)))
        .collect();
    let ranges: Vec<String> = headers
        .get_all(axum::http::header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|range| essence(range).to_owned())
        .collect();
    let by_accept = offered.iter().find(|(_, media)| {
        ranges
            .iter()
            .any(|r| r.eq_ignore_ascii_case(essence(media)))
    });
    crate::Asked {
        pairs,
        file: by_f.or(by_accept).map(|(f, _)| *f),
    }
}

/// A media type or range without its parameters.
fn essence(media: &str) -> &str {
    media.split(';').next().unwrap_or_default().trim()
}

#[cfg(any(feature = "parquet", feature = "tabular"))]
mod file {
    use super::ITEMS;
    use crate::model::Collection;
    use crate::{items, Here, Problem, Reply};
    use antares_export::{Column, Error, Kind, Sink};
    use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;
    use serde_json::Value;

    /// A writer of one collection's file: its name (a sheet's), its columns, whether it has a
    /// geometry, and the ceiling.
    pub(crate) type Make = fn(&str, Vec<Column>, bool, usize) -> Result<Box<dyn Sink>, Error>;

    /// The Parquet writer.
    #[cfg(feature = "parquet")]
    pub(crate) fn parquet(
        _: &str,
        columns: Vec<Column>,
        geometry: bool,
        max: usize,
    ) -> Result<Box<dyn Sink>, Error> {
        Ok(Box::new(antares_export::parquet::Writer::new(
            columns, geometry, max,
        )?))
    }

    /// The CSV writer.
    #[cfg(feature = "tabular")]
    pub(crate) fn csv(
        _: &str,
        columns: Vec<Column>,
        geometry: bool,
        max: usize,
    ) -> Result<Box<dyn Sink>, Error> {
        Ok(Box::new(antares_export::csv::Writer::new(
            columns, geometry, max,
        )?))
    }

    /// A workbook of the one sheet `name`.
    #[cfg(feature = "tabular")]
    pub(crate) fn xlsx(
        name: &str,
        columns: Vec<Column>,
        geometry: bool,
        max: usize,
    ) -> Result<Box<dyn Sink>, Error> {
        Ok(Box::new(antares_export::xlsx::Workbook::single(
            name, columns, geometry, max,
        )?))
    }

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
                    "the file could not be written",
                ),
            }
        }
    }

    /// A file to save: its media type, its name, its bytes.
    pub(crate) fn attachment(media: &str, name: &str, bytes: Vec<u8>) -> Reply {
        (
            StatusCode::OK,
            [
                (CONTENT_TYPE, media.to_owned()),
                (
                    CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ],
            bytes,
        )
            .into_response()
    }

    /// A column per property the view shows (those `properties` keeps), typed from the
    /// collection's schema.
    pub(crate) fn columns(c: &Collection, p: &items::Params) -> Vec<Column> {
        c.properties
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
            .collect()
    }

    impl Here<'_> {
        /// `items` of `c` under `pairs` as the file `f` names.
        pub(crate) async fn file(
            &self,
            c: &Collection,
            pairs: &[(String, String)],
            f: &str,
        ) -> Reply {
            match self.write_file(c, pairs, f).await {
                Ok((media, bytes)) => attachment(media, &format!("{}.{f}", c.id), bytes),
                Err(refused) => refused.reply(),
            }
        }

        async fn write_file(
            &self,
            c: &Collection,
            pairs: &[(String, String)],
            f: &str,
        ) -> Result<(&'static str, Vec<u8>), Problem> {
            let missing = Problem(StatusCode::NOT_ACCEPTABLE, "no such file");
            let (_, media, make) = ITEMS.iter().find(|e| e.0 == f).ok_or(missing)?;
            let mut p = items::params(c, pairs)?;
            if p.crs != crate::crs::CRS84 {
                return Err(Problem(
                    StatusCode::BAD_REQUEST,
                    "a file is written in CRS84: leave crs out or name CRS84",
                ));
            }
            let geometry = c.geometry.is_some();
            let mut sink = make(&c.id, columns(c, &p), geometry, self.max_bytes)?;
            let mut write = |page: &[Value]| sink.page(page);
            let rows = self.rows(c, &mut p, pairs, &mut write).await?;
            let provenance = self.provenance(&[c.id.as_str()], pairs, rows);
            Ok((media, sink.finish(&provenance)?))
        }

        /// Every page of `items` of `c` under `p`, each handed to `write` as it is read; the
        /// number of Features.
        pub(crate) async fn rows(
            &self,
            c: &Collection,
            p: &mut items::Params,
            pairs: &[(String, String)],
            write: &mut (dyn FnMut(&[Value]) -> Result<(), Error> + Send),
        ) -> Result<usize, Problem> {
            p.limit = items::MAX_LIMIT;
            let digest = items::digest(self.id, &c.id, pairs);
            let mut rows = 0usize;
            loop {
                let refs = self.refs(p.profile);
                let page = items::page(self.view, self.caller, c, p, digest, refs).await?;
                write(&page.features)?;
                rows = rows.saturating_add(page.features.len());
                // A token that does not move would read the same page again.
                let next = page.next.filter(|next| p.next.as_ref() != Some(next));
                if next.is_none() {
                    return Ok(rows);
                }
                p.next = next;
            }
        }

        /// What produced a file: the view, the collections, the query, the rows, when, and
        /// which broker.
        pub(crate) fn provenance(
            &self,
            collections: &[&str],
            pairs: &[(String, String)],
            rows: usize,
        ) -> Vec<(String, String)> {
            let query: Vec<String> = pairs.iter().map(|(n, v)| format!("{n}={v}")).collect();
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            [
                ("view", self.id.to_owned()),
                ("collections", collections.join(",")),
                ("query", query.join("&")),
                ("rows", rows.to_string()),
                ("exportedAt", now),
                ("broker", format!("antares {}", env!("CARGO_PKG_VERSION"))),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect()
        }
    }
}

#[cfg(feature = "tabular")]
mod bundle {
    use super::file::{attachment, columns};
    use crate::model::Collection;
    use crate::{items, Here, Problem, Reply};
    use antares_export::bundle::Bundle;
    use antares_export::xlsx::Workbook;
    use antares_export::Sink;
    use axum::http::StatusCode;
    use serde_json::{Map, Value};
    use std::fmt::Write as _;

    /// The filters a file of several collections takes, and those collections.
    type Chosen<'c> = (Vec<(String, String)>, Vec<&'c Collection>);

    impl Here<'_> {
        /// Every collection the view shows (or those `collections` names) as one file: `f=xlsx`
        /// a sheet each, `f=zip` a CSV each (and its Parquet in a `parquet` build), with a
        /// README and the provenance.
        pub(crate) async fn bundle(&self, pairs: &[(String, String)], f: &str) -> Reply {
            let written = match self.chosen(pairs) {
                Ok((filters, chosen)) if f == "zip" => self.zip(&filters, &chosen, pairs).await,
                Ok((filters, chosen)) => self.xlsx(&filters, &chosen, pairs).await,
                Err(refused) => Err(refused),
            };
            match written {
                Ok((media, bytes)) => attachment(media, &format!("{}.{f}", self.id), bytes),
                Err(refused) => refused.reply(),
            }
        }

        /// The filters every collection takes (`bbox`, `datetime`) and the collections named,
        /// all of them when `collections` is absent.
        fn chosen(&self, pairs: &[(String, String)]) -> Result<Chosen<'_>, Problem> {
            let mut filters = Vec::new();
            let mut chosen: Vec<&Collection> = self.collections.values().collect();
            for (name, value) in pairs {
                match name.as_str() {
                    "bbox" | "datetime" => filters.push((name.clone(), value.clone())),
                    "collections" => {
                        let unknown = Problem(
                            StatusCode::BAD_REQUEST,
                            "collections names collections of the view",
                        );
                        chosen = value
                            .split(',')
                            .map(|id| self.collections.get(id))
                            .collect::<Option<_>>()
                            .ok_or(unknown)?;
                    }
                    _ => {
                        return Err(Problem(
                            StatusCode::BAD_REQUEST,
                            "a file of several collections takes bbox, datetime and collections",
                        ))
                    }
                }
            }
            Ok((filters, chosen))
        }

        async fn xlsx(
            &self,
            filters: &[(String, String)],
            chosen: &[&Collection],
            pairs: &[(String, String)],
        ) -> Result<(&'static str, Vec<u8>), Problem> {
            let mut book = Box::new(Workbook::new(self.max_bytes));
            let mut rows = 0usize;
            for c in chosen {
                let mut p = items::params(c, filters)?;
                book.sheet(&c.id, columns(c, &p), c.geometry.is_some())?;
                let mut write = |page: &[Value]| book.page(page);
                let read = self.rows(c, &mut p, filters, &mut write).await?;
                rows = rows.saturating_add(read);
            }
            let ids: Vec<&str> = chosen.iter().map(|c| c.id.as_str()).collect();
            let provenance = self.provenance(&ids, pairs, rows);
            Ok((antares_export::xlsx::MEDIA_TYPE, book.finish(&provenance)?))
        }

        async fn zip(
            &self,
            filters: &[(String, String)],
            chosen: &[&Collection],
            pairs: &[(String, String)],
        ) -> Result<(&'static str, Vec<u8>), Problem> {
            let mut bundle = Bundle::new(self.max_bytes);
            let mut files = Vec::new();
            let mut rows = 0usize;
            for c in chosen {
                let mut p = items::params(c, filters)?;
                let (geometry, max) = (c.geometry.is_some(), self.max_bytes);
                let mut sinks: Vec<(&str, Box<dyn Sink>)> = vec![
                    (
                        "csv",
                        super::file::csv(&c.id, columns(c, &p), geometry, max)?,
                    ),
                    #[cfg(feature = "parquet")]
                    (
                        "parquet",
                        super::file::parquet(&c.id, columns(c, &p), geometry, max)?,
                    ),
                ];
                let mut write =
                    |page: &[Value]| sinks.iter_mut().try_for_each(|(_, s)| s.page(page));
                let read = self.rows(c, &mut p, filters, &mut write).await?;
                rows = rows.saturating_add(read);
                let provenance = self.provenance(&[c.id.as_str()], pairs, read);
                for (extension, sink) in sinks {
                    let name = format!("{}.{extension}", c.id);
                    bundle.add(&name, &sink.finish(&provenance)?)?;
                    files.push((name, extension));
                }
            }
            let ids: Vec<&str> = chosen.iter().map(|c| c.id.as_str()).collect();
            let provenance = self.provenance(&ids, pairs, rows);
            let record: Map<String, Value> = provenance
                .iter()
                .map(|(k, v)| (k.clone(), Value::from(v.as_str())))
                .collect();
            bundle.add(
                "provenance.json",
                Value::Object(record).to_string().as_bytes(),
            )?;
            bundle.add("README.md", readme(self.id, &provenance, &files).as_bytes())?;
            Ok((antares_export::bundle::MEDIA_TYPE, bundle.finish()?))
        }
    }

    /// What the bundle holds and where it came from, for a reader without the platform.
    fn readme(view: &str, provenance: &[(String, String)], files: &[(String, &str)]) -> String {
        let at = provenance
            .iter()
            .find(|(k, _)| k == "exportedAt")
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        let mut text = format!(
            "# {view}\n\nThe collections of the view `{view}`, exported at {at}: one file per \
             collection, one row per Feature.\n\n| File | Holds |\n|---|---|\n"
        );
        for (file, extension) in files {
            let holds = if *extension == "csv" {
                "the collection as CSV"
            } else {
                "the collection as (Geo)Parquet"
            };
            let _ = writeln!(text, "| `{file}` | {holds} |");
        }
        text.push_str(
            "| `provenance.json` | the view, the collections, the query, the rows, when, and \
             which broker |\n",
        );
        text
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

    const OFFERED: &[(&str, &str)] = &[
        ("parquet", "application/vnd.apache.parquet"),
        ("csv", "text/csv; charset=utf-8; header=present"),
    ];

    #[test]
    fn a_file_is_asked_by_f_or_by_accept() {
        let none = HeaderMap::new();
        let asked_for = asked(&none, pairs(&[("f", "csv"), ("limit", "5")]), OFFERED);
        assert_eq!(asked_for.file, Some("csv"));
        assert_eq!(
            asked_for.pairs,
            pairs(&[("limit", "5")]),
            "the f is the encoding, no filter"
        );
        let mut accept = HeaderMap::new();
        accept.insert(
            axum::http::header::ACCEPT,
            HeaderValue::from_static("application/json;q=0.5, TEXT/CSV;q=0.9"),
        );
        assert_eq!(asked(&accept, Vec::new(), OFFERED).file, Some("csv"));
        accept.insert(axum::http::header::ACCEPT, HeaderValue::from_static("*/*"));
        assert_eq!(
            asked(&accept, Vec::new(), OFFERED).file,
            None,
            "anything is GeoJSON"
        );
        let asked_for = asked(&none, pairs(&[("f", "gpkg")]), OFFERED);
        assert_eq!(asked_for.file, None);
        assert_eq!(
            asked_for.pairs,
            pairs(&[("f", "gpkg")]),
            "another f stays to be refused"
        );
    }

    #[test]
    fn f_asks_for_a_file_only_in_a_build_that_writes_it() {
        let offered: Vec<(&str, &str)> = ITEMS.iter().map(|e| (e.0, e.1)).collect();
        let asked_for = asked(&HeaderMap::new(), pairs(&[("f", "parquet")]), &offered);
        assert_eq!(asked_for.file.is_some(), cfg!(feature = "parquet"));
        assert_eq!(asked_for.pairs.is_empty(), asked_for.file.is_some());
    }

    #[cfg(any(feature = "parquet", feature = "tabular"))]
    #[test]
    fn a_file_the_writer_refuses_is_a_500_naming_nothing() {
        let refused = crate::Problem::from(antares_export::Error::Write("x".into()));
        assert_eq!(
            refused,
            crate::Problem(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "the file could not be written"
            )
        );
    }
}
