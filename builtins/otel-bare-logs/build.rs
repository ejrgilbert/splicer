use std::path::Path;

fn main() {
    builtin_manifest::build_helper::codegen(Path::new("manifest.toml"))
        .expect("manifest.toml codegen");
}
