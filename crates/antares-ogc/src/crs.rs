// SPDX-License-Identifier: EUPL-1.2
//! Part 2 (OGC 18-058r1, mapping rows R1 to R5): `CRS84` and `CRS84h` only, because no coordinate
//! transform is in the tree (`geo` relates, it does not reproject); any other CRS is 400.

/// `CRS84`, `GeoJSON`'s own and the storage CRS.
pub(crate) const CRS84: &str = "http://www.opengis.net/def/crs/OGC/1.3/CRS84";
/// `CRS84` with ellipsoidal height.
pub(crate) const CRS84H: &str = "http://www.opengis.net/def/crs/OGC/0/CRS84h";
/// The refusal of any other CRS: it lists the supported ones (R2).
pub(crate) const UNSUPPORTED: &str = "the supported CRS are http://www.opengis.net/def/crs/OGC/1.3/CRS84 and http://www.opengis.net/def/crs/OGC/0/CRS84h";

/// One of the supported CRS, by its URI or its safe CURIE (`[OGC:CRS84]`).
pub(crate) fn read(text: &str) -> Option<&'static str> {
    match text {
        CRS84 | "[OGC:CRS84]" => Some(CRS84),
        CRS84H | "[OGC:CRS84h]" => Some(CRS84H),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crs84_and_crs84h_by_uri_or_curie() {
        assert_eq!(read(CRS84), Some(CRS84));
        assert_eq!(read("[OGC:CRS84h]"), Some(CRS84H));
        assert_eq!(read("http://www.opengis.net/def/crs/EPSG/0/4326"), None);
    }
}
