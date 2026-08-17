//! Generic string-rewriting walk over a wasm-wave [`Value`].

use std::borrow::Cow;
use std::collections::HashMap;

use wasm_wave::value::{Type, Value};
use wasm_wave::wasm::{WasmType, WasmTypeKind, WasmValue};

/// Rebuild `value` (typed by `ty`) with `f` applied to every `string` leaf.
pub fn map_strings(value: &Value, ty: &Type, f: &impl Fn(&str) -> String) -> Value {
    match ty.kind() {
        WasmTypeKind::String => Value::make_string(Cow::Owned(f(&value.unwrap_string()))),

        WasmTypeKind::List => {
            let elem_ty = ty
                .list_element_type()
                .expect("list type exposes its element type");
            let mapped = value
                .unwrap_list()
                .map(|c| map_strings(&c, &elem_ty, f))
                .collect::<Vec<_>>();
            Value::make_list(ty, mapped).expect("mapped list matches its type")
        }

        WasmTypeKind::Record => {
            let field_types: HashMap<String, Type> = ty
                .record_fields()
                .map(|(n, t)| (n.into_owned(), t))
                .collect();
            let mut names: Vec<String> = Vec::new();
            let mut values: Vec<Value> = Vec::new();
            for (name, child) in value.unwrap_record() {
                let field_ty = field_types
                    .get(name.as_ref())
                    .expect("record value field is declared in its type");
                values.push(map_strings(&child, field_ty, f));
                names.push(name.into_owned());
            }
            let fields = names.iter().map(String::as_str).zip(values);
            Value::make_record(ty, fields).expect("mapped record matches its type")
        }

        WasmTypeKind::Tuple => {
            let elem_types: Vec<Type> = ty.tuple_element_types().collect();
            let mapped = value
                .unwrap_tuple()
                .zip(elem_types.iter())
                .map(|(c, t)| map_strings(&c, t, f))
                .collect::<Vec<_>>();
            Value::make_tuple(ty, mapped).expect("mapped tuple matches its type")
        }

        WasmTypeKind::Option => {
            let inner_ty = ty
                .option_some_type()
                .expect("option type exposes its some-type");
            let mapped = value.unwrap_option().map(|c| map_strings(&c, &inner_ty, f));
            Value::make_option(ty, mapped).expect("mapped option matches its type")
        }

        WasmTypeKind::Result => {
            let (ok_ty, err_ty) = ty.result_types().expect("result type exposes its arms");
            let map_arm = |payload: Option<Cow<'_, Value>>, arm_ty: &Option<Type>| {
                payload.map(|c| {
                    let t = arm_ty
                        .as_ref()
                        .expect("result payload present implies a declared arm type");
                    map_strings(&c, t, f)
                })
            };
            let mapped = match value.unwrap_result() {
                Ok(payload) => Ok(map_arm(payload, &ok_ty)),
                Err(payload) => Err(map_arm(payload, &err_ty)),
            };
            Value::make_result(ty, mapped).expect("mapped result matches its type")
        }

        WasmTypeKind::Variant => {
            let (case, payload) = value.unwrap_variant();
            let payload_ty = ty
                .variant_cases()
                .find(|(n, _)| n.as_ref() == case.as_ref())
                .and_then(|(_, t)| t);
            let mapped = payload.map(|c| {
                let t = payload_ty
                    .as_ref()
                    .expect("variant payload present implies a declared payload type");
                map_strings(&c, t, f)
            });
            Value::make_variant(ty, &case, mapped).expect("mapped variant matches its type")
        }

        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WitTyped;

    fn shout(v: &Value, ty: &Type) -> Value {
        map_strings(v, ty, &|s| s.to_uppercase())
    }

    #[test]
    fn maps_bare_string() {
        let ty = String::wave_type();
        let out = shout(&"hi".to_string().to_value(), &ty);
        assert_eq!(String::from_value(&out).unwrap(), "HI");
    }

    #[test]
    fn maps_strings_in_list() {
        let v = vec!["a".to_string(), "b".to_string()];
        let ty = Vec::<String>::wave_type();
        let out = shout(&v.to_value(), &ty);
        assert_eq!(
            Vec::<String>::from_value(&out).unwrap(),
            vec!["A".to_string(), "B".to_string()]
        );
    }

    #[test]
    fn maps_string_inside_option_and_leaves_numbers() {
        // tuple<option<string>, u32>: the string is rewritten, the u32
        // clones through untouched.
        let v: (Option<String>, u32) = (Some("secret".to_string()), 7);
        let ty = <(Option<String>, u32)>::wave_type();
        let out = shout(&v.to_value(), &ty);
        let (s, n) = <(Option<String>, u32)>::from_value(&out).unwrap();
        assert_eq!(s, Some("SECRET".to_string()));
        assert_eq!(n, 7);
    }

    #[test]
    fn maps_string_in_result_ok_arm() {
        let v: Result<String, u32> = Ok("ok".to_string());
        let ty = Result::<String, u32>::wave_type();
        let out = shout(&v.to_value(), &ty);
        assert_eq!(Result::<String, u32>::from_value(&out).unwrap(), Ok("OK".to_string()));
    }

    #[test]
    fn none_and_empty_are_untouched() {
        let none: Option<String> = None;
        let ty = Option::<String>::wave_type();
        let out = shout(&none.to_value(), &ty);
        assert_eq!(Option::<String>::from_value(&out).unwrap(), None);
    }
}
