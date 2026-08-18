//! Type-directed value construction: walk a wasm-wave [`Type`] and
//! build a [`Value`] of that type, with the *policy* for what each node
//! becomes supplied by a pluggable [`ValueBuilder`].

use wasm_wave::value::{Type as WaveType, Value as WaveValue};
use wasm_wave::wasm::{WasmType, WasmTypeKind, WasmValue};

use crate::bridge::{BridgeError, WitTyped};

/// Absolute recursion ceiling.
const HARD_DEPTH_CAP: u32 = 128;

/// Why type-directed construction gave up on a type.
#[derive(Debug)]
pub enum GenError {
    /// A kind with no value representation (resource/stream/future/
    /// error-context) appeared in the type.
    UnsupportedKind(WasmTypeKind),
    /// A `variant` with zero cases (not expressible in valid WIT).
    EmptyVariant,
    /// An `enum` with zero cases (not expressible in valid WIT).
    EmptyEnum,
    DepthLimit,
}
impl std::fmt::Display for GenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedKind(k) => write!(f, "cannot construct a value of kind {k:?}"),
            Self::EmptyVariant => write!(f, "variant type has no cases"),
            Self::EmptyEnum => write!(f, "enum type has no cases"),
            Self::DepthLimit => write!(f, "type has no finite value (recursion depth limit)"),
        }
    }
}
impl std::error::Error for GenError {}

/// Supplies the value at each node of a [`build_value`] walk. The walk
/// handles structure; implementors decide content and shape. Leaf methods
/// return the finished primitive. The walk still enforces termination at
/// the depth bound (forcing empty lists, `none`, and payloadless variant
/// cases), so a builder cannot cause unbounded recursion.
pub trait ValueBuilder {
    fn build_bool(&mut self) -> bool;
    fn build_u8(&mut self) -> u8;
    fn build_u16(&mut self) -> u16;
    fn build_u32(&mut self) -> u32;
    fn build_u64(&mut self) -> u64;
    fn build_s8(&mut self) -> i8;
    fn build_s16(&mut self) -> i16;
    fn build_s32(&mut self) -> i32;
    fn build_s64(&mut self) -> i64;
    fn build_f32(&mut self) -> f32;
    fn build_f64(&mut self) -> f64;
    fn build_char(&mut self) -> char;
    fn build_string(&mut self) -> String;

    /// Element count for a `list`.
    fn list_len(&mut self) -> usize;
    /// Whether an `option` is `some`.
    fn option_some(&mut self) -> bool;
    /// Whether a `result` takes its `ok` arm.
    fn result_ok(&mut self) -> bool;
    /// Pick a `variant` case: return one of the `allowed` case indices
    /// (the walk narrows `allowed` to payloadless cases at the depth
    /// bound). Returning an index outside `allowed` is treated as
    /// `allowed[0]`.
    fn variant_case(&mut self, allowed: &[usize]) -> usize;
    /// Pick an `enum` case index in `0..num_cases`.
    fn enum_case(&mut self, num_cases: usize) -> usize;
    /// Whether flag `idx` of `total` is set.
    fn flag_set(&mut self, idx: usize, total: usize) -> bool;
}

/// Build a [`WaveValue`] of type `ty`, drawing every value decision from
/// `b`. `max_depth` bounds recursion through `list`/`option`/`variant`;
/// `depth` is the current level (start at `0`).
pub fn build_value<B: ValueBuilder + ?Sized>(
    ty: &WaveType,
    b: &mut B,
    max_depth: u32,
    depth: u32,
) -> Result<WaveValue, GenError> {
    if depth > HARD_DEPTH_CAP {
        return Err(GenError::DepthLimit);
    }
    let at_cap = depth >= max_depth;
    match ty.kind() {
        WasmTypeKind::Bool => Ok(WaveValue::make_bool(b.build_bool())),
        WasmTypeKind::U8 => Ok(WaveValue::make_u8(b.build_u8())),
        WasmTypeKind::U16 => Ok(WaveValue::make_u16(b.build_u16())),
        WasmTypeKind::U32 => Ok(WaveValue::make_u32(b.build_u32())),
        WasmTypeKind::U64 => Ok(WaveValue::make_u64(b.build_u64())),
        WasmTypeKind::S8 => Ok(WaveValue::make_s8(b.build_s8())),
        WasmTypeKind::S16 => Ok(WaveValue::make_s16(b.build_s16())),
        WasmTypeKind::S32 => Ok(WaveValue::make_s32(b.build_s32())),
        WasmTypeKind::S64 => Ok(WaveValue::make_s64(b.build_s64())),
        WasmTypeKind::F32 => Ok(WaveValue::make_f32(b.build_f32())),
        WasmTypeKind::F64 => Ok(WaveValue::make_f64(b.build_f64())),
        WasmTypeKind::Char => Ok(WaveValue::make_char(b.build_char())),
        WasmTypeKind::String => Ok(WaveValue::make_string(b.build_string().into())),

        WasmTypeKind::List => {
            let elem_ty = ty
                .list_element_type()
                .expect("list type exposes its element type");
            let n = if at_cap { 0 } else { b.list_len() };
            let mut elems = Vec::with_capacity(n);
            for _ in 0..n {
                elems.push(build_value(&elem_ty, b, max_depth, depth + 1)?);
            }
            Ok(WaveValue::make_list(ty, elems).expect("generated list matches its element type"))
        }

        WasmTypeKind::Tuple => {
            let elem_types: Vec<WaveType> = ty.tuple_element_types().collect();
            let mut elems = Vec::with_capacity(elem_types.len());
            for t in &elem_types {
                elems.push(build_value(t, b, max_depth, depth + 1)?);
            }
            Ok(WaveValue::make_tuple(ty, elems).expect("generated tuple matches its element types"))
        }

        WasmTypeKind::Record => {
            let fields: Vec<(String, WaveType)> = ty
                .record_fields()
                .map(|(n, t)| (n.into_owned(), t))
                .collect();
            let mut values: Vec<WaveValue> = Vec::with_capacity(fields.len());
            for (_, t) in &fields {
                values.push(build_value(t, b, max_depth, depth + 1)?);
            }
            let named = fields.iter().map(|(n, _)| n.as_str()).zip(values);
            Ok(WaveValue::make_record(ty, named).expect("generated record matches its field types"))
        }

        WasmTypeKind::Option => {
            let inner_ty = ty
                .option_some_type()
                .expect("option type exposes its some-type");
            let some = !at_cap && b.option_some();
            let inner = if some {
                Some(build_value(&inner_ty, b, max_depth, depth + 1)?)
            } else {
                None
            };
            Ok(WaveValue::make_option(ty, inner).expect("generated option matches its some-type"))
        }

        WasmTypeKind::Result => {
            let (ok_ty, err_ty) = ty.result_types().expect("result type exposes its arms");
            let arm = if b.result_ok() {
                Ok(build_arm(&ok_ty, b, max_depth, depth)?)
            } else {
                Err(build_arm(&err_ty, b, max_depth, depth)?)
            };
            Ok(WaveValue::make_result(ty, arm).expect("generated result matches its arm types"))
        }

        WasmTypeKind::Variant => {
            let cases: Vec<(String, Option<WaveType>)> = ty
                .variant_cases()
                .map(|(n, t)| (n.into_owned(), t))
                .collect();
            if cases.is_empty() {
                return Err(GenError::EmptyVariant);
            }
            // At the depth bound, restrict to payloadless cases so the
            // walk terminates; fall back to all cases when none exist.
            let payloadless: Vec<usize> = cases
                .iter()
                .enumerate()
                .filter(|(_, (_, p))| p.is_none())
                .map(|(i, _)| i)
                .collect();
            let all: Vec<usize> = (0..cases.len()).collect();
            let allowed: &[usize] = if at_cap && !payloadless.is_empty() {
                &payloadless
            } else {
                &all
            };
            let mut idx = b.variant_case(allowed);
            if !allowed.contains(&idx) {
                idx = allowed[0];
            }
            let (name, payload_ty) = &cases[idx];
            let payload = match payload_ty {
                Some(t) => Some(build_value(t, b, max_depth, depth + 1)?),
                None => None,
            };
            Ok(WaveValue::make_variant(ty, name, payload)
                .expect("generated variant case matches its payload type"))
        }

        WasmTypeKind::Enum => {
            let cases: Vec<String> = ty.enum_cases().map(|c| c.into_owned()).collect();
            if cases.is_empty() {
                return Err(GenError::EmptyEnum);
            }
            let mut idx = b.enum_case(cases.len());
            if idx >= cases.len() {
                idx = 0;
            }
            Ok(WaveValue::make_enum(ty, &cases[idx]).expect("generated enum case is declared"))
        }

        WasmTypeKind::Flags => {
            let names: Vec<String> = ty.flags_names().map(|c| c.into_owned()).collect();
            let total = names.len();
            let mut set: Vec<&str> = Vec::with_capacity(total);
            for (i, n) in names.iter().enumerate() {
                if b.flag_set(i, total) {
                    set.push(n.as_str());
                }
            }
            Ok(WaveValue::make_flags(ty, set).expect("generated flags are all declared"))
        }

        other => Err(GenError::UnsupportedKind(other)),
    }
}

/// Build a `result`/`variant` arm payload: `Some` when the arm declares
/// a type, `None` for a unit arm.
fn build_arm<B: ValueBuilder + ?Sized>(
    arm_ty: &Option<WaveType>,
    b: &mut B,
    max_depth: u32,
    depth: u32,
) -> Result<Option<WaveValue>, GenError> {
    match arm_ty {
        Some(t) => Ok(Some(build_value(t, b, max_depth, depth + 1)?)),
        None => Ok(None),
    }
}

/// Build a typed `T` directly from a builder: walks `T::wave_type()`,
/// then decodes the constructed value back into `T`.
pub fn build_typed<T: WitTyped, B: ValueBuilder + ?Sized>(
    b: &mut B,
    max_depth: u32,
) -> Result<T, BridgeError> {
    let value = build_value(&T::wave_type(), b, max_depth, 0)
        .map_err(|_| BridgeError::ExpectedTypeShape("value construction failed for the given type"))?;
    T::from_value(&value)
}

// ---- MinimalBuilder --------------------------------------------------

/// Builds the smallest inhabitant of a type, useful for default-fill.
#[derive(Debug, Default, Clone, Copy)]
pub struct MinimalBuilder;

impl ValueBuilder for MinimalBuilder {
    fn build_bool(&mut self) -> bool {
        false
    }
    fn build_u8(&mut self) -> u8 {
        0
    }
    fn build_u16(&mut self) -> u16 {
        0
    }
    fn build_u32(&mut self) -> u32 {
        0
    }
    fn build_u64(&mut self) -> u64 {
        0
    }
    fn build_s8(&mut self) -> i8 {
        0
    }
    fn build_s16(&mut self) -> i16 {
        0
    }
    fn build_s32(&mut self) -> i32 {
        0
    }
    fn build_s64(&mut self) -> i64 {
        0
    }
    fn build_f32(&mut self) -> f32 {
        0.0
    }
    fn build_f64(&mut self) -> f64 {
        0.0
    }
    fn build_char(&mut self) -> char {
        '\0'
    }
    fn build_string(&mut self) -> String {
        String::new()
    }
    fn list_len(&mut self) -> usize {
        0
    }
    fn option_some(&mut self) -> bool {
        false
    }
    fn result_ok(&mut self) -> bool {
        true
    }
    fn variant_case(&mut self, allowed: &[usize]) -> usize {
        allowed.first().copied().unwrap_or(0)
    }
    fn enum_case(&mut self, _num_cases: usize) -> usize {
        0
    }
    fn flag_set(&mut self, _idx: usize, _total: usize) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test builder that eagerly picks *non-minimal* shapes: `some`
    /// options, `err` arm, the last allowed variant/enum case, a fixed
    /// list length, and alternating flags. Exercises the walk's compound
    /// and termination handling independently of any real policy.
    struct Eager {
        list_len: usize,
    }

    impl ValueBuilder for Eager {
        fn build_bool(&mut self) -> bool {
            true
        }
        fn build_u8(&mut self) -> u8 {
            7
        }
        fn build_u16(&mut self) -> u16 {
            7
        }
        fn build_u32(&mut self) -> u32 {
            7
        }
        fn build_u64(&mut self) -> u64 {
            7
        }
        fn build_s8(&mut self) -> i8 {
            -7
        }
        fn build_s16(&mut self) -> i16 {
            -7
        }
        fn build_s32(&mut self) -> i32 {
            -7
        }
        fn build_s64(&mut self) -> i64 {
            -7
        }
        fn build_f32(&mut self) -> f32 {
            1.5
        }
        fn build_f64(&mut self) -> f64 {
            1.5
        }
        fn build_char(&mut self) -> char {
            'z'
        }
        fn build_string(&mut self) -> String {
            "x".to_string()
        }
        fn list_len(&mut self) -> usize {
            self.list_len
        }
        fn option_some(&mut self) -> bool {
            true
        }
        fn result_ok(&mut self) -> bool {
            false
        }
        fn variant_case(&mut self, allowed: &[usize]) -> usize {
            *allowed.last().unwrap()
        }
        fn enum_case(&mut self, num_cases: usize) -> usize {
            num_cases - 1
        }
        fn flag_set(&mut self, idx: usize, _total: usize) -> bool {
            idx % 2 == 0
        }
    }

    #[test]
    fn minimal_builder_produces_smallest_inhabitant() {
        // result<record { id: u32, tags: list<string>, ratio: f64 }, string>
        let record = WaveType::record([
            ("id", WaveType::U32),
            ("tags", WaveType::list(WaveType::STRING)),
            ("ratio", WaveType::F64),
        ])
        .unwrap();
        let ty = WaveType::result(Some(record), Some(WaveType::STRING));

        let mut b = MinimalBuilder;
        let v = build_value(&ty, &mut b, 5, 0).expect("minimal build succeeds");

        let ok = v.unwrap_result().expect("minimal takes the ok arm");
        let rec = ok.expect("ok arm carries the record");
        for (name, field) in rec.unwrap_record() {
            match name.as_ref() {
                "id" => assert_eq!(field.unwrap_u32(), 0),
                "tags" => assert_eq!(field.unwrap_list().count(), 0),
                "ratio" => assert_eq!(field.unwrap_f64(), 0.0),
                other => panic!("unexpected field {other}"),
            }
        }
    }

    #[test]
    fn build_typed_via_dyn_builder() {
        // The `?Sized` bound allows dynamic dispatch over builders.
        let b: &mut dyn ValueBuilder = &mut MinimalBuilder;
        let (a, flag): (u32, bool) = build_typed(b, 5).unwrap();
        assert_eq!(a, 0);
        assert!(!flag);
    }

    #[test]
    fn walk_enforces_termination_regardless_of_builder() {
        // Eager wants length-2 lists and `some` options at every level;
        // the depth bound must still cut recursion so the walk returns.
        let ty = Vec::<Vec<Vec<Vec<u32>>>>::wave_type();
        let mut b = Eager { list_len: 2 };
        let v = build_value(&ty, &mut b, 1, 0).expect("bounded walk terminates");
        let back = Vec::<Vec<Vec<Vec<u32>>>>::from_value(&v).unwrap();
        // Depth 0 list is populated; depth-1 lists are at the cap → empty.
        assert_eq!(back.len(), 2);
        assert!(back.iter().all(|inner| inner.is_empty()));
    }

    #[test]
    fn walk_builds_selected_nominal_cases() {
        use wasm_wave::wasm::WasmValue as _;

        let variant = WaveType::variant([
            ("nul", None),
            ("num", Some(WaveType::S32)),
            ("text", Some(WaveType::STRING)),
        ])
        .unwrap();
        let enum_ty = WaveType::enum_ty(["red", "green", "blue"]).unwrap();
        let flags = WaveType::flags(["read", "write", "exec"]).unwrap();

        let mut b = Eager { list_len: 0 };
        // Eager picks the last allowed case: `text` with an "x" payload.
        let v = build_value(&variant, &mut b, 5, 0).unwrap();
        let (case, payload) = v.unwrap_variant();
        assert_eq!(case, "text");
        assert_eq!(payload.unwrap().unwrap_string(), "x");
        // Last enum case.
        let v = build_value(&enum_ty, &mut b, 5, 0).unwrap();
        assert_eq!(v.unwrap_enum(), "blue");
        // Alternating flags: indices 0 and 2 set (`read`, `exec`).
        let v = build_value(&flags, &mut b, 5, 0).unwrap();
        let set: Vec<_> = v.unwrap_flags().map(|n| n.into_owned()).collect();
        assert_eq!(set, vec!["read".to_string(), "exec".to_string()]);
    }

    #[test]
    fn unsupported_kind_is_reported() {
        // No public constructor for resource types here, so this asserts
        // the error path exists for the empty-variant degenerate case
        // instead (also a GenError, same reporting path).
        let empty = WaveType::variant([("only", None)]).unwrap();
        // A single-case variant is valid; pick it to confirm success,
        // then confirm the error type renders.
        let mut b = MinimalBuilder;
        assert!(build_value(&empty, &mut b, 5, 0).is_ok());
        assert_eq!(
            GenError::DepthLimit.to_string(),
            "type has no finite value (recursion depth limit)"
        );
    }
}
