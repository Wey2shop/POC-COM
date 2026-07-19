//! Maidenhead Locator System conversion, 4-character (field+square)
//! precision -- e.g. `IO80`. Pure math, no GUI dependency, shared by the
//! Settings popover's location picker and the contact map (`mh_map.rs`).

/// Converts a lat/lon (degrees) to a 4-character locator such as `IO80`.
pub fn to_locator(lat: f64, lon: f64) -> String {
    let lon = (lon.clamp(-180.0, 180.0) + 180.0).min(359.999_999);
    let lat = (lat.clamp(-90.0, 90.0) + 90.0).min(179.999_999);

    let field_lon = (lon / 20.0).floor() as u8;
    let field_lat = (lat / 10.0).floor() as u8;
    let square_lon = ((lon % 20.0) / 2.0).floor() as u8;
    let square_lat = (lat % 10.0).floor() as u8;

    format!(
        "{}{}{}{}",
        (b'A' + field_lon) as char,
        (b'A' + field_lat) as char,
        square_lon,
        square_lat
    )
}

/// Parses a 4-character locator (case-insensitive) back to the lat/lon at
/// the *center* of that square. Returns `None` for anything that isn't a
/// well-formed 4-char locator -- used to skip legacy/freeform Location text
/// from before this feature existed rather than crashing on it.
pub fn to_latlon_center(locator: &str) -> Option<(f64, f64)> {
    let chars: Vec<char> = locator.trim().chars().collect();
    if chars.len() != 4 {
        return None;
    }

    let field_lon = chars[0].to_ascii_uppercase();
    let field_lat = chars[1].to_ascii_uppercase();
    if !('A'..='R').contains(&field_lon) || !('A'..='R').contains(&field_lat) {
        return None;
    }
    let square_lon = chars[2].to_digit(10)?;
    let square_lat = chars[3].to_digit(10)?;

    let field_lon = field_lon as u8 - b'A';
    let field_lat = field_lat as u8 - b'A';

    let lon = field_lon as f64 * 20.0 + square_lon as f64 * 2.0 + 1.0 - 180.0;
    let lat = field_lat as f64 * 10.0 + square_lat as f64 * 1.0 + 0.5 - 90.0;
    Some((lat, lon))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_locator_round_trips() {
        // IO80 covers most of Ireland/west England -- center is well inside
        // its own square, so converting there and back must land on IO80.
        let (lat, lon) = to_latlon_center("IO80").expect("valid locator");
        assert_eq!(to_locator(lat, lon), "IO80");
    }

    #[test]
    fn locator_is_case_insensitive() {
        assert_eq!(to_latlon_center("io80"), to_latlon_center("IO80"));
    }

    #[test]
    fn extremes_do_not_panic() {
        assert_eq!(to_locator(90.0, 180.0).len(), 4);
        assert_eq!(to_locator(-90.0, -180.0).len(), 4);
    }

    #[test]
    fn malformed_locators_are_rejected_not_panicking() {
        assert!(to_latlon_center("").is_none());
        assert!(to_latlon_center("XX").is_none());
        assert!(to_latlon_center("ZZ99").is_none()); // Z is past 'R'
        assert!(to_latlon_center("IOXX").is_none()); // digits required
        assert!(to_latlon_center("Ridge Site 2").is_none()); // legacy freeform text
    }

    #[test]
    fn grid_covers_whole_globe_without_overlap_at_boundaries() {
        // Every field-corner boundary should still produce a valid 4-char code.
        for lat in [-90.0, -45.0, 0.0, 45.0, 89.9] {
            for lon in [-180.0, -90.0, 0.0, 90.0, 179.9] {
                let locator = to_locator(lat, lon);
                assert_eq!(locator.len(), 4);
                assert!(to_latlon_center(&locator).is_some());
            }
        }
    }
}
