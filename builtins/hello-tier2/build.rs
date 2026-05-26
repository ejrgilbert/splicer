use std::path::Path;

fn main() {
    builtin_protocol::build_helper::codegen(Path::new("manifest.toml"))
        .expect("manifest.toml codegen");
}
