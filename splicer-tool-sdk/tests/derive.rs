//! Integration tests for `#[derive(TypedFromCells)]`. Lives outside
//! `src/` so the derive's `::splicer_tool_sdk::...` absolute paths
//! resolve naturally (the test crate sees the SDK as an external dep).

use splicer_tool_sdk::{
    Cell, EnumInfo, FieldTree, RecordInfo, TypedFromCells, VariantInfo,
};

fn empty_tree(cells: Vec<Cell>, root: u32) -> FieldTree {
    FieldTree {
        cells,
        record_infos: vec![],
        flags_infos: vec![],
        enum_infos: vec![],
        variant_infos: vec![],
        handle_infos: vec![],
        root,
    }
}

// ---------- record (struct) -----------------------------------------------

#[derive(TypedFromCells, Debug, PartialEq)]
struct Point {
    x: u32,
    y: u32,
}

#[test]
fn record_two_fields() {
    let mut tree = empty_tree(
        vec![Cell::Integer(3), Cell::Integer(4), Cell::RecordOf(0)],
        2,
    );
    tree.record_infos.push(RecordInfo {
        type_name: "point".into(),
        fields: vec![("x".into(), 0), ("y".into(), 1)],
    });

    let p = Point::from_cells(&tree, tree.root).unwrap();
    assert_eq!(p, Point { x: 3, y: 4 });
}

#[derive(TypedFromCells, Debug, PartialEq)]
struct Pet {
    pet_name: String,
    age_years: u32,
}

#[test]
fn record_kebab_case_field_names() {
    let mut tree = empty_tree(
        vec![
            Cell::Text("Whiskers".into()),
            Cell::Integer(7),
            Cell::RecordOf(0),
        ],
        2,
    );
    // WIT field names are kebab-case; the derive converts the Rust
    // snake_case ident before looking up.
    tree.record_infos.push(RecordInfo {
        type_name: "pet".into(),
        fields: vec![("pet-name".into(), 0), ("age-years".into(), 1)],
    });

    let p = Pet::from_cells(&tree, tree.root).unwrap();
    assert_eq!(
        p,
        Pet {
            pet_name: "Whiskers".into(),
            age_years: 7
        }
    );
}

#[derive(TypedFromCells, Debug, PartialEq)]
struct Pair {
    head: Point,
    tail: Point,
}

#[test]
fn record_with_nested_record() {
    let mut tree = empty_tree(
        vec![
            Cell::Integer(1),    // 0: head.x
            Cell::Integer(2),    // 1: head.y
            Cell::Integer(3),    // 2: tail.x
            Cell::Integer(4),    // 3: tail.y
            Cell::RecordOf(0),   // 4: head -> record_infos[0]
            Cell::RecordOf(1),   // 5: tail -> record_infos[1]
            Cell::RecordOf(2),   // 6: pair -> record_infos[2]
        ],
        6,
    );
    tree.record_infos.push(RecordInfo {
        type_name: "point".into(),
        fields: vec![("x".into(), 0), ("y".into(), 1)],
    });
    tree.record_infos.push(RecordInfo {
        type_name: "point".into(),
        fields: vec![("x".into(), 2), ("y".into(), 3)],
    });
    tree.record_infos.push(RecordInfo {
        type_name: "pair".into(),
        fields: vec![("head".into(), 4), ("tail".into(), 5)],
    });

    let p = Pair::from_cells(&tree, tree.root).unwrap();
    assert_eq!(
        p,
        Pair {
            head: Point { x: 1, y: 2 },
            tail: Point { x: 3, y: 4 },
        }
    );
}

#[test]
fn record_missing_field_errors() {
    let mut tree = empty_tree(vec![Cell::Integer(3), Cell::RecordOf(0)], 1);
    tree.record_infos.push(RecordInfo {
        type_name: "point".into(),
        // missing `y`
        fields: vec![("x".into(), 0)],
    });
    assert!(Point::from_cells(&tree, tree.root).is_err());
}

// ---------- enum (unit-only) ----------------------------------------------

#[derive(TypedFromCells, Debug, PartialEq)]
enum Color {
    Red,
    Green,
    Blue,
}

#[test]
fn unit_enum_via_enum_case() {
    let mut tree = empty_tree(vec![Cell::EnumCase(0)], 0);
    tree.enum_infos.push(EnumInfo {
        type_name: "color".into(),
        case_name: "green".into(),
    });
    assert_eq!(Color::from_cells(&tree, tree.root).unwrap(), Color::Green);
}

#[test]
fn unit_enum_via_variant_case_compat() {
    // A WIT `variant` whose cases happen to all be unit produces
    // `Cell::VariantCase`; the unit-enum derive accepts both.
    let mut tree = empty_tree(vec![Cell::VariantCase(0)], 0);
    tree.variant_infos.push(VariantInfo {
        type_name: "color".into(),
        case_name: "blue".into(),
        payload: None,
    });
    assert_eq!(Color::from_cells(&tree, tree.root).unwrap(), Color::Blue);
}

#[test]
fn unit_enum_unknown_case_errors() {
    let mut tree = empty_tree(vec![Cell::EnumCase(0)], 0);
    tree.enum_infos.push(EnumInfo {
        type_name: "color".into(),
        case_name: "violet".into(),
    });
    assert!(Color::from_cells(&tree, tree.root).is_err());
}

// ---------- variant (mixed unit + payload) --------------------------------

#[derive(TypedFromCells, Debug, PartialEq)]
enum Event {
    Click(Point),
    Hover,
    Close(u32),
}

#[test]
fn variant_unit_case() {
    let mut tree = empty_tree(vec![Cell::VariantCase(0)], 0);
    tree.variant_infos.push(VariantInfo {
        type_name: "event".into(),
        case_name: "hover".into(),
        payload: None,
    });
    assert_eq!(Event::from_cells(&tree, tree.root).unwrap(), Event::Hover);
}

#[test]
fn variant_payload_case() {
    let mut tree = empty_tree(
        vec![Cell::Integer(99), Cell::VariantCase(0)],
        1,
    );
    tree.variant_infos.push(VariantInfo {
        type_name: "event".into(),
        case_name: "close".into(),
        payload: Some(0),
    });
    assert_eq!(Event::from_cells(&tree, tree.root).unwrap(), Event::Close(99));
}

#[test]
fn variant_record_payload() {
    let mut tree = empty_tree(
        vec![
            Cell::Integer(10),    // 0
            Cell::Integer(20),    // 1
            Cell::RecordOf(0),    // 2 -> point
            Cell::VariantCase(0), // 3 -> click(point)
        ],
        3,
    );
    tree.record_infos.push(RecordInfo {
        type_name: "point".into(),
        fields: vec![("x".into(), 0), ("y".into(), 1)],
    });
    tree.variant_infos.push(VariantInfo {
        type_name: "event".into(),
        case_name: "click".into(),
        payload: Some(2),
    });
    let e = Event::from_cells(&tree, tree.root).unwrap();
    assert_eq!(e, Event::Click(Point { x: 10, y: 20 }));
}

#[test]
fn variant_payload_mismatch_errors() {
    // Payload-carrying case sent with no payload.
    let mut tree = empty_tree(vec![Cell::VariantCase(0)], 0);
    tree.variant_infos.push(VariantInfo {
        type_name: "event".into(),
        case_name: "close".into(),
        payload: None,
    });
    assert!(Event::from_cells(&tree, tree.root).is_err());

    // Unit case sent with a payload.
    let mut tree = empty_tree(vec![Cell::Integer(0), Cell::VariantCase(0)], 1);
    tree.variant_infos.push(VariantInfo {
        type_name: "event".into(),
        case_name: "hover".into(),
        payload: Some(0),
    });
    assert!(Event::from_cells(&tree, tree.root).is_err());
}

// ---------- wrong-shape errors --------------------------------------------

#[test]
fn record_decode_of_non_record_cell_errors() {
    let tree = empty_tree(vec![Cell::Bool(true)], 0);
    assert!(Point::from_cells(&tree, 0).is_err());
}

#[test]
fn enum_decode_of_non_enum_cell_errors() {
    let tree = empty_tree(vec![Cell::Bool(true)], 0);
    assert!(Color::from_cells(&tree, 0).is_err());
}
