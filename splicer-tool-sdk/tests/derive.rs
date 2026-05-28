//! Behavioral tests for `#[derive(WitTyped)]`, over both type
//! populations it serves: types written by hand and types emitted by a
//! consumer's own `wit_bindgen::generate!` call (via
//! `additional_derives`). Each is exercised through the wave round trip
//! (`to_value` / `from_value`) and, for handwritten types, the cells
//! decode path (`cells_to_typed`).

use splicer_tool_sdk::{
    cells_to_typed, Cell, EnumInfo, FieldTree, RecordInfo, VariantInfo, WitTyped,
};
use splicer_tool_sdk_derive::WitTyped as DeriveWitTyped;

// ---- handwritten types ----------------------------------------------

#[derive(Debug, PartialEq, DeriveWitTyped)]
struct Pet {
    pet_name: String,
    age_years: u32,
    favorite: Option<u32>,
    status: Status,
}

#[derive(Debug, PartialEq, DeriveWitTyped)]
enum Status {
    Healthy,
    UnderObservation,
}

#[derive(Debug, PartialEq, DeriveWitTyped)]
enum Outcome {
    NotFound,
    Found(u32),
}

#[derive(Debug, PartialEq, DeriveWitTyped)]
struct Wrapper<T> {
    inner: T,
}

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

#[test]
fn record_round_trips_through_wave() {
    let pet = Pet {
        pet_name: "Whiskers".to_string(),
        age_years: 7,
        favorite: Some(3),
        status: Status::Healthy,
    };
    let v = pet.to_value();
    let back = Pet::from_value(&v).unwrap();
    assert_eq!(back, pet);
}

#[test]
fn unit_enum_round_trips_through_wave() {
    for s in [Status::Healthy, Status::UnderObservation] {
        let v = s.to_value();
        let back = Status::from_value(&v).unwrap();
        assert_eq!(back, s);
    }
}

#[test]
fn variant_round_trips_through_wave() {
    for o in [Outcome::NotFound, Outcome::Found(42)] {
        let v = o.to_value();
        let back = Outcome::from_value(&v).unwrap();
        assert_eq!(back, o);
    }
}

#[test]
fn generic_wrapper_round_trips_through_wave() {
    let w = Wrapper {
        inner: "hello".to_string(),
    };
    let v = w.to_value();
    let back: Wrapper<String> = WitTyped::from_value(&v).unwrap();
    assert_eq!(back, w);
}

#[test]
fn unit_enum_case_name_is_kebab() {
    // `UnderObservation` must lower to the WIT case `under-observation`
    // so it lines up with what the wire format and codegen produce.
    let mut t = empty_tree(vec![Cell::EnumCase(0)], 0);
    t.enum_infos.push(EnumInfo {
        type_name: "status".into(),
        case_name: "under-observation".into(),
    });
    let s: Status = cells_to_typed(&t, t.root).unwrap();
    assert_eq!(s, Status::UnderObservation);
}

#[test]
fn record_decodes_from_cells() {
    // pet { pet-name: "Whiskers", age-years: 7, favorite: some(3),
    //       status: healthy }
    let mut t = empty_tree(
        vec![
            Cell::Text("Whiskers".into()), // 0: pet-name
            Cell::Integer(7),              // 1: age-years
            Cell::Integer(3),              // 2: favorite inner
            Cell::OptionSome(2),           // 3: favorite
            Cell::EnumCase(0),             // 4: status
            Cell::RecordOf(0),             // 5: root
        ],
        5,
    );
    t.record_infos.push(RecordInfo {
        type_name: "pet".into(),
        fields: vec![
            ("pet-name".into(), 0),
            ("age-years".into(), 1),
            ("favorite".into(), 3),
            ("status".into(), 4),
        ],
    });
    t.enum_infos.push(EnumInfo {
        type_name: "status".into(),
        case_name: "healthy".into(),
    });

    let pet: Pet = cells_to_typed(&t, t.root).unwrap();
    assert_eq!(
        pet,
        Pet {
            pet_name: "Whiskers".into(),
            age_years: 7,
            favorite: Some(3),
            status: Status::Healthy,
        }
    );
}

#[test]
fn variant_payload_decodes_from_cells() {
    // outcome::found(42)
    let mut t = empty_tree(vec![Cell::Integer(42), Cell::VariantCase(0)], 1);
    t.variant_infos.push(VariantInfo {
        type_name: "outcome".into(),
        case_name: "found".into(),
        payload: Some(0),
    });
    let o: Outcome = cells_to_typed(&t, t.root).unwrap();
    assert_eq!(o, Outcome::Found(42));
}

// ---- wit-bindgen-generated types (additional_derives) ----------------
//
// A consumer that owns its `wit_bindgen::generate!` call attaches the
// derive to every generated type via `additional_derives` rather than
// hand-editing generated code. The world only imports its interface,
// which keeps the bindings host-compilable here. (wit-bindgen prunes a
// record/variant no function references, so `describe` keeps all three
// types alive; it is never called.)

mod generated {
    wit_bindgen::generate!({
        inline: r#"
            package test:wittyped@0.1.0;
            interface shapes {
                record point { x: u32, y: u32 }
                enum color { red, green, blue }
                variant shape { circle(u32), unit-square }
                describe: func(p: point, c: color, s: shape);
            }
            world derive-host {
                import shapes;
            }
        "#,
        world: "derive-host",
        additional_derives: [splicer_tool_sdk_derive::WitTyped],
        generate_all,
    });
}

use generated::test::wittyped::shapes::{Color, Point, Shape};

#[test]
fn generated_record_round_trips() {
    let p = Point { x: 3, y: 4 };
    let back = Point::from_value(&p.to_value()).unwrap();
    assert_eq!((back.x, back.y), (3, 4));
}

#[test]
fn generated_enum_round_trips() {
    let back = Color::from_value(&Color::Green.to_value()).unwrap();
    assert!(matches!(back, Color::Green));
}

#[test]
fn generated_variant_round_trips() {
    let back = Shape::from_value(&Shape::Circle(7).to_value()).unwrap();
    assert!(matches!(back, Shape::Circle(7)));

    let back = Shape::from_value(&Shape::UnitSquare.to_value()).unwrap();
    assert!(matches!(back, Shape::UnitSquare));
}
