use fastrand::Rng;
use splicer_tool_sdk::ValueBuilder;

#[derive(Debug, Clone)]
pub struct GenConfig {
    pub max_depth: u32,
    pub max_list_len: u32,
    pub max_string_len: u32,
    pub boundary_bias: f32,
}

pub struct FuzzBuilder {
    rng: Rng,
    boundary_bias: f32,
    max_list_len: u32,
    max_string_len: u32,
}
impl FuzzBuilder {
    pub fn new(seed: u64, cfg: &GenConfig) -> Self {
        Self {
            rng: Rng::with_seed(seed),
            boundary_bias: cfg.boundary_bias,
            max_list_len: cfg.max_list_len,
            max_string_len: cfg.max_string_len,
        }
    }

    /// Whether this leaf takes an edge-case value.
    fn boundary(&mut self) -> bool {
        self.rng.f32() < self.boundary_bias.clamp(0.0, 1.0)
    }

    /// Empty or max-length.
    fn biased_len(&mut self, max: u32) -> usize {
        let len = if self.boundary() {
            if self.rng.bool() {
                0
            } else {
                max
            }
        } else {
            self.rng.u32(0..=max)
        };
        len as usize
    }
}

/// Returns a random integer of `$ty`: with `boundary_bias`.
macro_rules! fuzz_int {
    ($method:ident, $rng:ident, $ty:ty) => {
        fn $method(&mut self) -> $ty {
            if self.boundary() {
                let choices: [$ty; 4] = [<$ty>::MIN, 0, 1, <$ty>::MAX];
                choices[self.rng.usize(0..choices.len())]
            } else {
                self.rng.$rng(..)
            }
        }
    };
}

impl ValueBuilder for FuzzBuilder {
    fn build_bool(&mut self) -> bool {
        self.rng.bool()
    }

    fuzz_int!(build_u8, u8, u8);
    fuzz_int!(build_u16, u16, u16);
    fuzz_int!(build_u32, u32, u32);
    fuzz_int!(build_u64, u64, u64);
    fuzz_int!(build_s8, i8, i8);
    fuzz_int!(build_s16, i16, i16);
    fuzz_int!(build_s32, i32, i32);
    fuzz_int!(build_s64, i64, i64);

    fn build_f32(&mut self) -> f32 {
        if self.boundary() {
            let specials = [
                0.0f32,
                -0.0,
                1.0,
                -1.0,
                f32::NAN,
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::MIN,
                f32::MAX,
                f32::EPSILON,
            ];
            specials[self.rng.usize(0..specials.len())]
        } else {
            // Any bit pattern, so NaN/inf/subnormals show up too.
            f32::from_bits(self.rng.u32(..))
        }
    }
    fn build_f64(&mut self) -> f64 {
        if self.boundary() {
            let specials = [
                0.0f64,
                -0.0,
                1.0,
                -1.0,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::MIN,
                f64::MAX,
                f64::EPSILON,
            ];
            specials[self.rng.usize(0..specials.len())]
        } else {
            f64::from_bits(self.rng.u64(..))
        }
    }
    fn build_char(&mut self) -> char {
        if self.boundary() {
            // Null, ASCII edges, last BMP scalar before the surrogate
            // gap, first astral scalar, and the max scalar value.
            let specials = ['\0', ' ', '~', '\u{7f}', '\u{d7ff}', '\u{e000}', '\u{10ffff}'];
            specials[self.rng.usize(0..specials.len())]
        } else {
            self.rng.char(..)
        }
    }
    fn build_string(&mut self) -> String {
        let len = self.biased_len(self.max_string_len);
        (0..len).map(|_| self.rng.char(..)).collect()
    }
    fn list_len(&mut self) -> usize {
        self.biased_len(self.max_list_len)
    }
    fn option_some(&mut self) -> bool {
        self.rng.bool()
    }
    fn result_ok(&mut self) -> bool {
        self.rng.bool()
    }
    fn variant_case(&mut self, allowed: &[usize]) -> usize {
        allowed[self.rng.usize(0..allowed.len())]
    }
    fn enum_case(&mut self, num_cases: usize) -> usize {
        self.rng.usize(0..num_cases.max(1))
    }
    fn flag_set(&mut self, _idx: usize, _total: usize) -> bool {
        self.rng.bool()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splicer_tool_sdk::{build_typed, WitTyped};

    fn cfg() -> GenConfig {
        GenConfig {
            max_depth: 5,
            max_list_len: 8,
            max_string_len: 32,
            boundary_bias: 0.3,
        }
    }

    fn gen<T: WitTyped>(seed: u64) -> T {
        let mut b = FuzzBuilder::new(seed, &cfg());
        build_typed::<T, _>(&mut b, cfg().max_depth).expect("generation succeeds")
    }

    #[test]
    fn primitives_and_compounds_round_trip() {
        for seed in 0..64 {
            let _: bool = gen(seed);
            let _: u64 = gen(seed);
            let _: i8 = gen(seed);
            let _: f64 = gen(seed);
            let _: char = gen(seed);
            let _: String = gen(seed);
            let _: Vec<u32> = gen(seed);
            let _: Option<String> = gen(seed);
            let _: Result<u32, String> = gen(seed);
            let _: (u32, String, Option<u8>) = gen(seed);
            let _: Vec<Option<Vec<u8>>> = gen(seed);
        }
    }

    #[test]
    fn deterministic_for_same_seed() {
        let a: Result<Vec<u32>, String> = gen(12345);
        let b: Result<Vec<u32>, String> = gen(12345);
        assert_eq!(a, b);
    }

    #[test]
    fn varies_across_seeds() {
        let mut seen = std::collections::HashSet::new();
        for seed in 0..64 {
            let v: (u32, String) = gen(seed);
            seen.insert(format!("{v:?}"));
        }
        assert!(seen.len() > 1, "generator produced only one distinct value");
    }

    #[test]
    fn empty_and_max_string_reachable_under_full_bias() {
        let c = GenConfig {
            boundary_bias: 1.0,
            max_string_len: 4,
            ..cfg()
        };
        let mut lens = std::collections::HashSet::new();
        for seed in 0..64 {
            let mut b = FuzzBuilder::new(seed, &c);
            let s: String = build_typed::<String, _>(&mut b, c.max_depth).unwrap();
            lens.insert(s.chars().count());
        }
        assert!(lens.contains(&0), "empty string boundary never hit");
        assert!(lens.contains(&4), "max-length string boundary never hit");
    }
}
