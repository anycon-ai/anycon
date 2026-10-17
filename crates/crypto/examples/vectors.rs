//! 生成跨实现测试向量（JSON）。`tests/interop.mjs` 会读它，用 wasm 对一遍。
//!
//! 用法：
//!   cargo run -q -p anycon-crypto --example vectors              # 打印到 stdout
//!   cargo run -q -p anycon-crypto --example vectors -- out.json  # 写文件
//!   ANYCON_VECTORS_OUT=dir cargo run -q -p anycon-crypto --example vectors   # 写 dir/vectors.json
//!
//! 内容：
//! - `kat`：确定性的已知答案（短码 → PSK、公钥 → 指纹、规范化、非法短码、RFC 7748 的 X25519 密钥对）。
//!   PSK 的期望值用 Node 24 的 `crypto.argon2Sync` 独立算、指纹用 Python `hashlib.blake2s`，不是 Rust 自己对自己。
//! - `pairing`：一次 Rust↔Rust 的 XXpsk3 完整录像（含双方静态私钥、PSK、三条握手消息、握手后双向各一条密文）。
//!   wasm 侧可以扮演 host（responder）读第一条消息并拿到 payload，证明 Rust 产出的字节 wasm 认得。
//!   后两条依赖对端的临时密钥，没法静态重放，完整的双向握手见 `interop_peer.rs`。
//!
//! 这些私钥只用于测试，不要拿去别处。

use anycon_crypto::{derive_psk, fingerprint, generate_pair_code, hex, Handshake, Keypair, Role};

fn j(s: &str) -> String {
    // 向量里只有 hex / 字母数字 / 少量符号；这里手写转义避免给 example 加依赖。
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn main() {
    let psk_codes = ["ABCD-2345", "23456789", "xyz9-kmnp"];
    let psk_items: Vec<String> = psk_codes
        .iter()
        .map(|c| format!("{{\"code\":{},\"psk\":{}}}", j(c), j(&hex(&derive_psk(c).unwrap()))))
        .collect();

    let fp_keys: Vec<[u8; 32]> = vec![core::array::from_fn(|i| i as u8), [0u8; 32], [0xffu8; 32]];
    let fp_items: Vec<String> = fp_keys
        .iter()
        .map(|k| format!("{{\"public\":{},\"fingerprint\":{}}}", j(&hex(k)), j(&fingerprint(k))))
        .collect();

    let normalize = [("abcd-2345", "ABCD2345"), ("  ab cd - 23 45\n", "ABCD2345"), ("", "")];
    let normalize_items: Vec<String> =
        normalize.iter().map(|(i, o)| format!("{{\"input\":{},\"output\":{}}}", j(i), j(o))).collect();

    let invalid = [
        "",
        "ABCD",
        "ABCD-234",
        "ABCD-23456",
        "ABCD-2340",
        "ABCD-2341",
        "ABCI-2345",
        "ABCL-2345",
        "ABCO-2345",
        "ABCD-234!",
    ];
    let invalid_items: Vec<String> = invalid.iter().map(|c| j(c)).collect();

    // RFC 7748 § 6.1，Alice。
    let rfc_secret = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
    let rfc_public = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";

    // 配对录像。
    let code = generate_pair_code();
    let psk = derive_psk(&code).unwrap();
    let joiner = Keypair::generate();
    let host = Keypair::generate();
    let mut i = Handshake::pairing(Role::Initiator, &joiner, &psk).unwrap();
    let mut r = Handshake::pairing(Role::Responder, &host, &psk).unwrap();
    let payload1 = b"anycon-interop-m1";
    let m1 = i.write(payload1).unwrap();
    assert_eq!(r.read(&m1).unwrap(), payload1);
    let m2 = r.write(b"").unwrap();
    i.read(&m2).unwrap();
    let m3 = i.write(b"").unwrap();
    r.read(&m3).unwrap();
    let mut it = i.into_transport().unwrap();
    let mut rt = r.into_transport().unwrap();
    let msg_i2r = b"hello host, from the Rust joiner";
    let msg_r2i = b"hello joiner, from the Rust host";
    let ct_i2r = it.encrypt(msg_i2r).unwrap();
    let ct_r2i = rt.encrypt(msg_r2i).unwrap();
    assert_eq!(rt.decrypt(&ct_i2r).unwrap(), msg_i2r);
    assert_eq!(it.decrypt(&ct_r2i).unwrap(), msg_r2i);

    let json = format!(
        concat!(
            "{{\n",
            "  \"version\": 1,\n",
            "  \"kat\": {{\n",
            "    \"derive_psk\": [{psk}],\n",
            "    \"fingerprint\": [{fp}],\n",
            "    \"normalize\": [{norm}],\n",
            "    \"invalid_codes\": [{invalid}],\n",
            "    \"x25519\": {{\"secret\":{rs},\"public\":{rp}}}\n",
            "  }},\n",
            "  \"pairing\": {{\n",
            "    \"code\": {code},\n",
            "    \"psk\": {psk_hex},\n",
            "    \"joiner\": {{\"secret\":{js},\"public\":{jp}}},\n",
            "    \"host\": {{\"secret\":{hs},\"public\":{hp}}},\n",
            "    \"m1_payload\": {p1},\n",
            "    \"m1\": {m1},\n",
            "    \"m2\": {m2},\n",
            "    \"m3\": {m3},\n",
            "    \"joiner_to_host\": {{\"plaintext\":{pi},\"ciphertext\":{ci}}},\n",
            "    \"host_to_joiner\": {{\"plaintext\":{pr},\"ciphertext\":{cr}}}\n",
            "  }}\n",
            "}}\n"
        ),
        psk = psk_items.join(","),
        fp = fp_items.join(","),
        norm = normalize_items.join(","),
        invalid = invalid_items.join(","),
        rs = j(rfc_secret),
        rp = j(rfc_public),
        code = j(&code),
        psk_hex = j(&hex(&psk)),
        js = j(&hex(&joiner.secret)),
        jp = j(&hex(&joiner.public)),
        hs = j(&hex(&host.secret)),
        hp = j(&hex(&host.public)),
        p1 = j(&hex(payload1)),
        m1 = j(&hex(&m1)),
        m2 = j(&hex(&m2)),
        m3 = j(&hex(&m3)),
        pi = j(&hex(msg_i2r)),
        ci = j(&hex(&ct_i2r)),
        pr = j(&hex(msg_r2i)),
        cr = j(&hex(&ct_r2i)),
    );

    let target = std::env::args().nth(1).or_else(|| {
        std::env::var("ANYCON_VECTORS_OUT").ok().map(|d| {
            std::fs::create_dir_all(&d).expect("create ANYCON_VECTORS_OUT");
            format!("{d}/vectors.json")
        })
    });
    match target {
        Some(path) => {
            std::fs::write(&path, json).expect("write vectors");
            eprintln!("wrote {path}");
        }
        None => print!("{json}"),
    }
}
