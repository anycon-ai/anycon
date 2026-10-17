//! `cargo run -p anycon-protocol --bin anycon-gen-schema -- [out_path]`
//! 默认写到 `schema/protocol.json`（仓库根）。CI 用 `just gen && git diff --exit-code` 校验。

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "schema/protocol.json".to_string());
    let schema = anycon_protocol::schema::generate();
    let text = serde_json::to_string_pretty(&schema).expect("json") + "\n";
    if let Some(parent) = std::path::Path::new(&out).parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(&out, text).expect("write schema");
    eprintln!("wrote {out}");
    let methods_out = std::path::Path::new(&out).with_file_name("methods.json");
    let methods = serde_json::to_string_pretty(&anycon_protocol::schema::generate_methods()).expect("json") + "\n";
    std::fs::write(&methods_out, methods).expect("write methods");
    eprintln!("wrote {}", methods_out.display());
}
