// SPDX-License-Identifier: EUPL-1.2
//! Office Open XML workbooks (T-4477, EP-44, EP-45): one sheet per collection, named by it, its
//! columns from that collection's schema, and a `provenance` sheet of what produced the file.
//! Written by hand as the gateway did, because a grid of inline strings and numbers is the whole
//! of what is needed and `SpreadsheetML` says how to write one in a page: a JSON number is a
//! numeric cell a reader can sum, anything else an inline string. Each sheet goes into the
//! archive row by row, compressed as it is written; the XML and each part's headers are counted
//! against the ceiling as they go in, so the archive, deflated, stays under it.

use std::fmt::Write as _;
use std::io::{Cursor, Write};

use serde_json::Value;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::{cells, distinct, guarded, header, Column, Error, Sink};

/// The media type of a workbook.
pub const MEDIA_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// The sheet the provenance pairs go to; a collection of that name gets a suffix.
const PROVENANCE: &str = "provenance";

/// A workbook being written: its sheets one after the other, the rows of the open one a page
/// at a time.
pub struct Workbook {
    archive: ZipWriter<Cursor<Vec<u8>>>,
    /// The data sheets' names, in order.
    names: Vec<String>,
    /// The open sheet's columns, geometry and last row number.
    columns: Vec<Column>,
    geometry: bool,
    row: u32,
    open: bool,
    written: usize,
    max: usize,
}

impl Workbook {
    /// An empty workbook refusing to grow past `max_bytes`.
    #[must_use]
    pub fn new(max_bytes: usize) -> Self {
        Self {
            archive: ZipWriter::new(Cursor::new(Vec::new())),
            names: Vec::new(),
            columns: Vec::new(),
            geometry: false,
            row: 0,
            open: false,
            written: 0,
            max: max_bytes,
        }
    }

    /// A workbook of one sheet named `name`, open for its pages.
    ///
    /// # Errors
    /// As [`Self::sheet`].
    pub fn single(
        name: &str,
        columns: Vec<Column>,
        geometry: bool,
        max_bytes: usize,
    ) -> Result<Self, Error> {
        let mut book = Self::new(max_bytes);
        book.sheet(name, columns, geometry)?;
        Ok(book)
    }

    /// Closes the open sheet and opens the next, named after `name` (Excel's rules: no
    /// `[]:*?/\`, at most 31 characters, unique without regard to case), with its header row.
    ///
    /// # Errors
    /// [`Error::TooLarge`] past the ceiling, [`Error::Write`] when the archive refuses a part.
    pub fn sheet(&mut self, name: &str, columns: Vec<Column>, geometry: bool) -> Result<(), Error> {
        self.close()?;
        let name = sheet_name(name, &self.names);
        self.names.push(name);
        let part = format!("xl/worksheets/sheet{}.xml", self.names.len());
        self.start(part)?;
        self.put(SHEET_HEAD)?;
        self.columns = distinct(columns);
        self.geometry = geometry;
        self.row = 0;
        self.open = true;
        let header = header(&self.columns, geometry);
        self.put_row(&header.iter().collect::<Vec<_>>())
    }

    /// Ends the open sheet's XML, if one is open.
    fn close(&mut self) -> Result<(), Error> {
        if self.open {
            self.put(SHEET_TAIL)?;
            self.open = false;
        }
        Ok(())
    }

    fn put_row(&mut self, cells: &[&Value]) -> Result<(), Error> {
        self.row = self.row.saturating_add(1);
        let row = sheet_row(self.row, cells);
        self.put(&row)
    }

    /// Opens the next part, its headers counted against the ceiling.
    fn start(&mut self, path: String) -> Result<(), Error> {
        self.charge(crate::PART_OVERHEAD)?;
        self.archive.start_file(path, options())?;
        Ok(())
    }

    /// Writes into the open part, counting the XML against the ceiling first.
    fn put(&mut self, xml: &str) -> Result<(), Error> {
        self.charge(xml.len())?;
        self.archive.write_all(xml.as_bytes())?;
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        self.written = self.written.saturating_add(bytes);
        if self.written > self.max {
            return Err(Error::TooLarge);
        }
        Ok(())
    }
}

impl Sink for Workbook {
    /// Rows of the open sheet.
    fn page(&mut self, features: &[Value]) -> Result<(), Error> {
        for feature in features {
            let row = cells(feature, &self.columns, self.geometry);
            self.put_row(&row)?;
        }
        Ok(())
    }

    /// The provenance sheet, the workbook's parts, and the archive's directory.
    fn finish(mut self: Box<Self>, provenance: &[(String, String)]) -> Result<Vec<u8>, Error> {
        self.close()?;
        let part = format!(
            "xl/worksheets/sheet{}.xml",
            self.names.len().saturating_add(1)
        );
        self.start(part)?;
        self.put(SHEET_HEAD)?;
        self.row = 0;
        for (key, value) in provenance {
            self.put_row(&[&Value::from(key.as_str()), &Value::from(value.as_str())])?;
        }
        self.put(SHEET_TAIL)?;
        let mut names = self.names.clone();
        names.push(PROVENANCE.to_owned());
        for (path, xml) in parts(&names) {
            self.start(path)?;
            self.put(&xml)?;
        }
        Ok(self.archive.finish()?.into_inner())
    }
}

pub(crate) fn options() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(CompressionMethod::Deflated)
}

/// A sheet name Excel opens: the characters it forbids as `_`, at most 31 characters, empty
/// as `sheet`, and a `~2`, `~3`, … suffix while it equals a name taken (case aside) or the
/// provenance sheet's.
fn sheet_name(name: &str, taken: &[String]) -> String {
    let clean: String = name
        .chars()
        .map(|c| if "[]:*?/\\".contains(c) { '_' } else { c })
        .take(31)
        .collect();
    let base = if clean.is_empty() {
        "sheet".to_owned()
    } else {
        clean
    };
    let used = |candidate: &str| {
        candidate.eq_ignore_ascii_case(PROVENANCE)
            || taken.iter().any(|t| t.eq_ignore_ascii_case(candidate))
    };
    let mut candidate = base.clone();
    let mut n = 1u32;
    while used(&candidate) {
        n = n.saturating_add(1);
        let suffix = format!("~{n}");
        let room = 31usize.saturating_sub(suffix.chars().count());
        candidate = format!("{}{suffix}", base.chars().take(room).collect::<String>());
    }
    candidate
}

/// One worksheet row: numbers as numbers, a null as no cell, anything else an inline string.
fn sheet_row(number: u32, cells: &[&Value]) -> String {
    let mut out = format!(r#"<row r="{number}">"#);
    for (index, value) in cells.iter().enumerate() {
        let reference = format!("{}{number}", column_name(index));
        match value {
            Value::Null => {}
            Value::Number(n) => {
                let _ = write!(out, r#"<c r="{reference}"><v>{n}</v></c>"#);
            }
            other => {
                let text = escaped(&guarded(other));
                let _ = write!(
                    out,
                    r#"<c r="{reference}" t="inlineStr"><is><t xml:space="preserve">{text}</t></is></c>"#
                );
            }
        }
    }
    out.push_str("</row>");
    out
}

/// The spreadsheet name of a zero-based column: `A`, `Z`, `AA`, `AB`, …
fn column_name(index: usize) -> String {
    let mut name = Vec::new();
    let mut rest = index;
    loop {
        let letter = u8::try_from(rest % 26).unwrap_or_default();
        name.push(char::from(b'A'.saturating_add(letter)));
        if rest < 26 {
            break;
        }
        rest = (rest / 26).saturating_sub(1);
    }
    name.iter().rev().collect()
}

/// XML text: the five predefined entities, and the control characters XML 1.0 forbids dropped,
/// because a workbook carrying one does not open at all.
fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            control if control < ' ' || control == '\u{7f}' => {}
            plain => out.push(plain),
        }
    }
    out
}

/// The workbook's own parts for `names`, its sheets in order.
fn parts(names: &[String]) -> Vec<(String, String)> {
    let (mut overrides, mut sheets, mut relations) = (String::new(), String::new(), String::new());
    for (i, name) in names.iter().enumerate() {
        let i = i.saturating_add(1);
        let name = escaped(name);
        let _ = write!(
            overrides,
            r#"<Override PartName="/xl/worksheets/sheet{i}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#
        );
        let _ = write!(
            sheets,
            r#"<sheet name="{name}" sheetId="{i}" r:id="rId{i}"/>"#
        );
        let _ = write!(
            relations,
            r#"<Relationship Id="rId{i}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{i}.xml"/>"#
        );
    }
    vec![
        (
            "[Content_Types].xml".to_owned(),
            format!(
                r#"{XML}<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>{overrides}</Types>"#
            ),
        ),
        (
            "_rels/.rels".to_owned(),
            format!(
                r#"{XML}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#
            ),
        ),
        (
            "xl/workbook.xml".to_owned(),
            format!(
                r#"{XML}<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>{sheets}</sheets></workbook>"#
            ),
        ),
        (
            "xl/_rels/workbook.xml.rels".to_owned(),
            format!(
                r#"{XML}<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{relations}</Relationships>"#
            ),
        ),
    ]
}

const XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
"#;

/// What a worksheet part holds before and after its rows.
const SHEET_HEAD: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>"#;
const SHEET_TAIL: &str = "</sheetData></worksheet>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Kind;
    use serde_json::json;
    use std::io::Read;

    type Outcome = Result<(), Box<dyn std::error::Error>>;

    fn part(book: &[u8], name: &str) -> Result<String, Box<dyn std::error::Error>> {
        let mut archive = zip::ZipArchive::new(Cursor::new(book))?;
        let mut text = String::new();
        archive.by_name(name)?.read_to_string(&mut text)?;
        Ok(text)
    }

    fn speed() -> Vec<Column> {
        vec![Column {
            name: "speed".into(),
            kind: Kind::Double,
        }]
    }

    #[test]
    fn a_sheet_per_collection_and_the_provenance_last() -> Outcome {
        let mut book = Box::new(Workbook::new(1 << 20));
        book.sheet("Car", speed(), true)?;
        book.page(&[
            json!({"id": "urn:a", "geometry": {"type": "Point", "coordinates": [1, 2]},
                           "properties": {"speed": 12.5}}),
        ])?;
        book.sheet("Stop", Vec::new(), false)?;
        book.page(&[json!({"id": "=cmd", "properties": {}})])?;
        let bytes = book.finish(&[("view".into(), "city & co".into())])?;
        let workbook = part(&bytes, "xl/workbook.xml")?;
        assert!(workbook.contains(r#"<sheet name="Car" sheetId="1" r:id="rId1"/><sheet name="Stop" sheetId="2" r:id="rId2"/><sheet name="provenance" sheetId="3" r:id="rId3"/>"#), "{workbook}");
        let car = part(&bytes, "xl/worksheets/sheet1.xml")?;
        assert!(car.ends_with(SHEET_TAIL));
        assert!(car.contains(r#"<row r="1"><c r="A1" t="inlineStr"><is><t xml:space="preserve">id</t></is></c><c r="B1" t="inlineStr"><is><t xml:space="preserve">geometry</t></is></c><c r="C1" t="inlineStr"><is><t xml:space="preserve">speed</t></is></c></row>"#), "{car}");
        assert!(
            car.contains(r#"<c r="C2"><v>12.5</v></c>"#),
            "a number is a numeric cell: {car}"
        );
        assert!(
            car.contains("{&quot;coordinates&quot;:[1,2],&quot;type&quot;:&quot;Point&quot;}"),
            "{car}"
        );
        let stop = part(&bytes, "xl/worksheets/sheet2.xml")?;
        assert!(
            stop.contains("<t xml:space=\"preserve\">&apos;=cmd</t>"),
            "a formula stays text: {stop}"
        );
        let provenance = part(&bytes, "xl/worksheets/sheet3.xml")?;
        assert!(provenance.contains("city &amp; co"), "{provenance}");
        let types = part(&bytes, "[Content_Types].xml")?;
        assert!(types.contains("/xl/worksheets/sheet3.xml"), "{types}");
        let rels = part(&bytes, "xl/_rels/workbook.xml.rels")?;
        assert!(rels.contains(r#"Id="rId3""#), "{rels}");
        assert!(part(&bytes, "_rels/.rels")?.contains("xl/workbook.xml"));
        Ok(())
    }

    #[test]
    fn a_workbook_of_no_collection_still_opens() -> Outcome {
        let bytes = Box::new(Workbook::new(1 << 20)).finish(&[])?;
        let workbook = part(&bytes, "xl/workbook.xml")?;
        assert!(
            workbook
                .contains(r#"<sheets><sheet name="provenance" sheetId="1" r:id="rId1"/></sheets>"#),
            "{workbook}"
        );
        let single = Box::new(Workbook::single("Car", speed(), false, 1 << 20)?).finish(&[])?;
        assert!(part(&single, "xl/workbook.xml")?.contains(r#"name="Car""#));
        Ok(())
    }

    #[test]
    fn past_the_ceiling_the_workbook_is_refused() -> Outcome {
        assert_eq!(
            Workbook::single("Car", speed(), false, 10).err(),
            Some(Error::TooLarge)
        );
        let mut book = Box::new(Workbook::single("Car", speed(), false, 2000)?);
        let rows: Vec<Value> = (0..100)
            .map(|i| json!({"id": format!("urn:{i}")}))
            .collect();
        assert_eq!(book.page(&rows), Err(Error::TooLarge));
        // Each part's headers count: the archive never outgrows what was allowed.
        let book = Box::new(Workbook::new(1 << 20)).finish(&[])?;
        let exact = SHEET_HEAD.len()
            + SHEET_TAIL.len()
            + 5 * crate::PART_OVERHEAD
            + parts(&[PROVENANCE.to_owned()])
                .iter()
                .map(|(_, x)| x.len())
                .sum::<usize>();
        assert!(book.len() <= exact, "{} > {exact}", book.len());
        assert!(Box::new(Workbook::new(exact)).finish(&[]).is_ok());
        let short = Box::new(Workbook::new(exact - 1)).finish(&[]);
        assert_eq!(short, Err(Error::TooLarge));
        Ok(())
    }

    #[test]
    fn sheet_names_follow_excels_rules() {
        let taken = vec!["Car".to_owned()];
        assert_eq!(sheet_name("car", &taken), "car~2");
        assert_eq!(sheet_name("Provenance", &[]), "Provenance~2");
        assert_eq!(sheet_name("a/b[c]:d*e?f\\g", &[]), "a_b_c__d_e_f_g");
        assert_eq!(sheet_name("", &[]), "sheet");
        let long = "x".repeat(40);
        assert_eq!(sheet_name(&long, &[]).chars().count(), 31);
        let taken = vec!["x".repeat(31), format!("{}~2", "x".repeat(29))];
        assert_eq!(sheet_name(&long, &taken), format!("{}~3", "x".repeat(29)));
    }

    #[test]
    fn column_names_carry_past_z() {
        for (index, name) in [
            (0, "A"),
            (25, "Z"),
            (26, "AA"),
            (27, "AB"),
            (51, "AZ"),
            (52, "BA"),
            (701, "ZZ"),
            (702, "AAA"),
        ] {
            assert_eq!(column_name(index), name);
        }
    }

    #[test]
    fn a_control_character_never_reaches_the_workbook() {
        assert_eq!(escaped("a\u{1}b\u{7f}"), "ab");
        assert_eq!(
            escaped("<a & 'b'>\"\t"),
            "&lt;a &amp; &apos;b&apos;&gt;&quot;\t"
        );
        assert_eq!(escaped("line\nbreak\r"), "line\nbreak\r");
    }
}
