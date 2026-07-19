//! One-time dev tool: converts `assets/land_data/ne_110m_land.geojson`
//! (Natural Earth 1:110m land polygons, public domain -- see
//! https://www.naturalearthdata.com/) into a compact flat binary
//! (`assets/land_data/land_outlines.bin`) that `mh_map.rs` embeds via
//! `include_bytes!` and reads with plain byte parsing at runtime -- no
//! JSON dependency ships in the actual app. Re-run this (`cargo run --bin
//! convert_land_geojson`) only if the source GeoJSON is ever updated.
//!
//! GeoJSON's `coordinates` values are nothing but nested arrays of
//! numbers (no strings/objects inside them), so this only needs a tiny
//! recursive parser for that shape, not a general JSON parser.

use std::fmt::Write as _;

#[derive(Debug)]
enum Node {
    Number(f64),
    Array(Vec<Node>),
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && (s[i] as char).is_whitespace() {
        i += 1;
    }
    i
}

/// Parses one JSON value (number or array of values) starting at `i`,
/// returning the parsed node and the index just past it.
fn parse_node(s: &[u8], i: usize) -> (Node, usize) {
    let i = skip_ws(s, i);
    if s[i] == b'[' {
        let mut i = i + 1;
        let mut items = Vec::new();
        loop {
            i = skip_ws(s, i);
            if s[i] == b']' {
                return (Node::Array(items), i + 1);
            }
            let (node, next) = parse_node(s, i);
            items.push(node);
            i = skip_ws(s, next);
            if s[i] == b',' {
                i += 1;
            }
        }
    } else {
        let start = i;
        let mut i = i;
        while i < s.len() && matches!(s[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
            i += 1;
        }
        let text = std::str::from_utf8(&s[start..i]).unwrap();
        (Node::Number(text.parse().unwrap_or_else(|e| panic!("bad number {text:?}: {e}"))), i)
    }
}

/// Walks the parsed coordinate tree collecting every "ring" -- an array
/// whose elements are all 2-element `[lon, lat]` positions -- regardless
/// of how deep it's nested (Polygon = array-of-rings, MultiPolygon =
/// array-of-array-of-rings). Stored as (lat, lon) to match this project's
/// convention elsewhere (`maidenhead.rs`, `mh_map.rs`).
fn collect_rings(node: &Node, out: &mut Vec<Vec<(f32, f32)>>) {
    match node {
        Node::Array(items) => {
            let is_ring = !items.is_empty()
                && items.iter().all(|item| matches!(item, Node::Array(pair) if pair.len() == 2 && pair.iter().all(|n| matches!(n, Node::Number(_)))));
            if is_ring {
                let ring = items
                    .iter()
                    .map(|item| {
                        let Node::Array(pair) = item else { unreachable!() };
                        let (Node::Number(lon), Node::Number(lat)) = (&pair[0], &pair[1]) else { unreachable!() };
                        (*lat as f32, *lon as f32)
                    })
                    .collect();
                out.push(ring);
            } else {
                for item in items {
                    collect_rings(item, out);
                }
            }
        }
        Node::Number(_) => {}
    }
}

fn main() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let in_path = format!("{manifest_dir}/assets/land_data/ne_110m_land.geojson");
    let out_path = format!("{manifest_dir}/assets/land_data/land_outlines.bin");

    let text = std::fs::read_to_string(&in_path).unwrap_or_else(|e| panic!("read {in_path}: {e}"));
    let bytes = text.as_bytes();

    let mut rings: Vec<Vec<(f32, f32)>> = Vec::new();
    let needle = b"\"coordinates\":";
    let mut pos = 0;
    while let Some(found) = find(bytes, needle, pos) {
        let value_start = found + needle.len();
        let (node, next) = parse_node(bytes, value_start);
        collect_rings(&node, &mut rings);
        pos = next;
    }

    let mut out = Vec::new();
    out.extend_from_slice(&(rings.len() as u32).to_le_bytes());
    let mut total_points = 0usize;
    for ring in &rings {
        out.extend_from_slice(&(ring.len() as u32).to_le_bytes());
        for &(lat, lon) in ring {
            out.extend_from_slice(&lat.to_le_bytes());
            out.extend_from_slice(&lon.to_le_bytes());
        }
        total_points += ring.len();
    }
    std::fs::write(&out_path, &out).unwrap_or_else(|e| panic!("write {out_path}: {e}"));

    let mut summary = String::new();
    let _ = write!(summary, "{} rings, {} points, {} bytes -> {out_path}", rings.len(), total_points, out.len());
    println!("{summary}");
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..].windows(needle.len()).position(|w| w == needle).map(|p| p + from)
}
