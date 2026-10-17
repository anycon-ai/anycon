//! crypto 单元测试：配对 / 重连握手、分片传输、短码、指纹。

use super::*;

const CODE: &str = "ABCD-2345";

fn pairing_pair(code: &str) -> (Handshake, Handshake, Keypair, Keypair) {
    let joiner_kp = Keypair::generate();
    let host_kp = Keypair::generate();
    let psk = derive_psk(code).unwrap();
    let joiner = Handshake::pairing(Role::Initiator, &joiner_kp, &psk).unwrap();
    let host = Handshake::pairing(Role::Responder, &host_kp, &psk).unwrap();
    (joiner, host, joiner_kp, host_kp)
}

/// 跑完三条消息的配对握手，返回 (joiner transport, host transport, joiner 公钥, host 公钥)。
fn pair_transports() -> (Transport, Transport, [u8; 32], [u8; 32]) {
    let (mut joiner, mut host, jk, hk) = pairing_pair(CODE);
    let m1 = joiner.write(b"").unwrap();
    host.read(&m1).unwrap();
    let m2 = host.write(b"").unwrap();
    joiner.read(&m2).unwrap();
    let m3 = joiner.write(b"").unwrap();
    host.read(&m3).unwrap();
    (joiner.into_transport().unwrap(), host.into_transport().unwrap(), jk.public, hk.public)
}

fn reconnect_transports() -> (Transport, Transport, Keypair, Keypair) {
    let client = Keypair::generate();
    let daemon = Keypair::generate();
    let mut ci = Handshake::reconnect(Role::Initiator, &client, Some(&daemon.public)).unwrap();
    let mut dr = Handshake::reconnect(Role::Responder, &daemon, None).unwrap();
    let m1 = ci.write(b"").unwrap();
    dr.read(&m1).unwrap();
    let m2 = dr.write(b"").unwrap();
    ci.read(&m2).unwrap();
    (ci.into_transport().unwrap(), dr.into_transport().unwrap(), client, daemon)
}

#[test]
fn handshake_and_transport_are_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Handshake>();
    assert_send::<Transport>();
    assert_send::<Keypair>();
}

// ---------- Keypair ----------

#[test]
fn keypair_from_secret_matches_rfc7748() {
    // RFC 7748 § 6.1，Alice 的密钥对。
    let secret: [u8; 32] =
        from_hex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a").unwrap().try_into().unwrap();
    let kp = Keypair::from_secret(secret);
    assert_eq!(hex(&kp.public), "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a");
    assert_eq!(kp.secret, secret);
}

#[test]
fn keypair_generate_is_random_and_consistent() {
    let a = Keypair::generate();
    let b = Keypair::generate();
    assert_ne!(a.secret, b.secret);
    assert_ne!(a.public, b.public);
    assert_eq!(Keypair::from_secret(a.secret).public, a.public);
}

#[test]
fn keypair_debug_hides_secret() {
    let kp = Keypair::generate();
    let dbg = format!("{kp:?}");
    assert!(dbg.contains(&hex(&kp.public)));
    assert!(!dbg.contains(&hex(&kp.secret)));
}

#[test]
fn keypair_secret_is_zeroized_on_drop() {
    let kp = Keypair::from_secret([7u8; 32]);
    let mut slot = std::mem::MaybeUninit::new(kp);
    // SAFETY: 值已经初始化；drop_in_place 之后只读取原始字节（不再当作 Keypair 使用），用来验证 Drop 清零了 secret。
    unsafe {
        let ptr = slot.as_mut_ptr();
        std::ptr::drop_in_place(ptr);
        let secret = std::ptr::read_volatile(std::ptr::addr_of!((*ptr).secret));
        assert_eq!(secret, [0u8; 32]);
    }
}

// ---------- 短码 / PSK / 指纹 ----------

#[test]
fn alphabet_matches_protocol_crate() {
    assert_eq!(std::str::from_utf8(PAIR_CODE_ALPHABET).unwrap(), anycon_protocol::PAIR_CODE_ALPHABET);
    assert_eq!(PAIR_CODE_LEN, anycon_protocol::PAIR_CODE_LEN);
}

#[test]
fn generate_pair_code_format() {
    let alphabet = anycon_protocol::PAIR_CODE_ALPHABET;
    for _ in 0..200 {
        let code = generate_pair_code();
        assert_eq!(code.len(), 9, "{code}");
        assert_eq!(code.as_bytes()[4], b'-', "{code}");
        assert!(code.chars().enumerate().all(|(i, c)| i == 4 || alphabet.contains(c)), "{code}");
        // 自己生成的码一定能通过校验。
        derive_psk(&code).unwrap();
    }
}

#[test]
fn generate_pair_code_uses_whole_alphabet_and_is_not_constant() {
    let mut seen = std::collections::HashSet::new();
    let mut codes = std::collections::HashSet::new();
    for _ in 0..2000 {
        let code = generate_pair_code();
        seen.extend(code.chars().filter(|c| *c != '-'));
        codes.insert(code);
    }
    assert_eq!(seen.len(), PAIR_CODE_ALPHABET.len(), "16000 个字符里应该见到字母表里的每个字符");
    assert!(codes.len() > 1990);
}

#[test]
fn normalize_code_cases() {
    assert_eq!(normalize_code("abcd-2345"), "ABCD2345");
    assert_eq!(normalize_code("  ab cd - 23 45\n"), "ABCD2345");
    assert_eq!(normalize_code(""), "");
}

#[test]
fn derive_psk_known_answers() {
    // Argon2id(password = 短码, salt = "anycon-pair-v2", m = 65536 KiB, t = 3, p = 1, out = 64)：
    // [..8] → match_id，[32..64] → PSK。期望值由独立实现（Node 24 自带的 `crypto.argon2Sync`）算出，
    // 之后参数任何变化都会让这里红。
    let s = crate::derive_pair_secrets("ABCD2345").unwrap();
    assert_eq!(s.match_id, "94d67d781538d997");
    assert_eq!(hex(&s.psk), "23e8b63dba0b3c479ab554d3fd58d8346b68799bbf88cf96418123133a9f6012");
    let s = crate::derive_pair_secrets("23456789").unwrap();
    assert_eq!(s.match_id, "f9f3c6c9940049ac");
    assert_eq!(hex(&s.psk), "17d04f19b36f2f987faf376e4962f2b51f3b764b1733c17e6297bc9c9751bb20");
    // 单独的两个入口和合并入口一致。
    assert_eq!(derive_psk("ABCD2345").unwrap(), crate::derive_pair_secrets("ABCD2345").unwrap().psk);
    assert_eq!(crate::derive_match_id("ABCD2345").unwrap(), crate::derive_pair_secrets("ABCD2345").unwrap().match_id);
}

#[test]
fn pair_kdf_is_deliberately_slow() {
    // 对离线穷举的防线：恶意 hub 拿到 match_id 后每猜一个短码都要付这么多。下限取得很保守（真实值 ~0.1–0.3 s），
    // 只为了防止有人把参数改小；上限不测（CI 机器快慢不一）。
    let t = std::time::Instant::now();
    crate::derive_pair_secrets("ABCD-2345").unwrap();
    let elapsed = t.elapsed();
    assert!(elapsed >= std::time::Duration::from_millis(20), "pair KDF too fast: {elapsed:?}");
    assert_eq!(crate::PAIR_KDF_M_COST_KIB, 64 * 1024);
    assert_eq!(crate::PAIR_KDF_T_COST, 3);
}

#[test]
fn derive_psk_normalizes_input() {
    let want = derive_psk("ABCD2345").unwrap();
    assert_eq!(derive_psk("ABCD-2345").unwrap(), want);
    assert_eq!(derive_psk("abcd-2345").unwrap(), want);
    assert_eq!(derive_psk(" abcd 2345 ").unwrap(), want);
}

#[test]
fn derive_psk_rejects_invalid_codes() {
    for bad in [
        "",
        "ABCD",
        "ABCD-234",         // 7 位
        "ABCD-23456",       // 9 位
        "ABCD-2340",        // 0 不在字母表
        "ABCD-2341",        // 1 不在字母表
        "ABCI-2345",        // I
        "ABCL-2345",        // L
        "ABCO-2345",        // O
        "ABCD-234!",        // 符号
        "ABCD_2345",        // 下划线不是分隔符
        "ÄBCD-2345",        // 非 ASCII
        "ＡＢＣＤ２３４５", // 全角
    ] {
        assert!(matches!(derive_psk(bad), Err(Error::InvalidCode)), "{bad:?} 应该被拒绝");
    }
}

#[test]
fn fingerprint_format_and_known_answer() {
    let fp = fingerprint(&core::array::from_fn(|i| i as u8));
    assert_eq!(fp, "05:82:56:07:d7:fd:f2:d8"); // Python hashlib.blake2s 对照
    assert_eq!(fingerprint(&[0u8; 32]), "32:0b:5e:a9:9e:65:3b:c2");
    let random = fingerprint(&Keypair::generate().public);
    assert_eq!(random.len(), 8 * 2 + 7);
    assert!(
        random.split(':').count() == 8
            && random
                .split(':')
                .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    );
}

#[test]
fn hex_roundtrip_and_strictness() {
    assert_eq!(from_hex(&hex(&[0, 1, 0xab, 0xff])).unwrap(), vec![0, 1, 0xab, 0xff]);
    assert_eq!(from_hex("").unwrap(), Vec::<u8>::new());
    assert!(from_hex("abc").is_err());
    assert!(from_hex("zz").is_err());
    assert!(from_hex("+f").is_err());
    assert!(from_hex("éé").is_err()); // 多字节字符不能 panic
}

// ---------- 配对握手 ----------

#[test]
fn pairing_handshake_success_and_remote_public_at_each_stage() {
    let (mut joiner, mut host, jk, hk) = pairing_pair(CODE);

    // 初始：谁都不知道对方公钥，initiator 先写。
    assert!(joiner.is_my_turn() && !host.is_my_turn());
    assert!(joiner.remote_public().is_none() && host.remote_public().is_none());
    assert!(!joiner.is_finished() && !host.is_finished());

    // XX 的第一条是 `-> e`，responder 读完仍不知道 initiator 的静态公钥。
    let m1 = joiner.write(b"").unwrap();
    assert_eq!(m1.len(), 32 + 16); // e + 空 payload 的 tag（psk 模式下 e 会 MixKey，payload 带 tag）
    assert!(!joiner.is_my_turn());
    host.read(&m1).unwrap();
    assert!(host.is_my_turn());
    assert!(host.remote_public().is_none());

    // 第二条 `<- e, ee, s, es`：initiator 读完知道了 host 的公钥。
    let m2 = host.write(b"").unwrap();
    joiner.read(&m2).unwrap();
    assert_eq!(joiner.remote_public(), Some(hk.public));
    assert!(joiner.is_my_turn());
    assert!(host.remote_public().is_none());
    assert!(!joiner.is_finished() && !host.is_finished());

    // 第三条 `-> s, se, psk`：写完 initiator 完成；host 读完才知道 joiner 的公钥并完成。
    let m3 = joiner.write(b"").unwrap();
    assert!(joiner.is_finished());
    assert!(!host.is_finished());
    assert!(host.remote_public().is_none());
    host.read(&m3).unwrap();
    assert!(host.is_finished());
    assert_eq!(host.remote_public(), Some(jk.public));
    assert!(!joiner.is_my_turn() && !host.is_my_turn());

    let mut jt = joiner.into_transport().unwrap();
    let mut ht = host.into_transport().unwrap();
    assert_eq!(jt.remote_public(), hk.public);
    assert_eq!(ht.remote_public(), jk.public);

    let ct = jt.encrypt(b"hello host").unwrap();
    assert_eq!(ht.decrypt(&ct).unwrap(), b"hello host");
    let ct = ht.encrypt(b"hello joiner").unwrap();
    assert_eq!(jt.decrypt(&ct).unwrap(), b"hello joiner");
}

#[test]
fn pairing_handshake_carries_payloads() {
    let (mut joiner, mut host, _, _) = pairing_pair(CODE);
    let m1 = joiner.write(b"hi").unwrap();
    assert_eq!(host.read(&m1).unwrap(), b"hi");
    let m2 = host.write(b"device-name:daemon").unwrap();
    assert_eq!(joiner.read(&m2).unwrap(), b"device-name:daemon");
    let m3 = joiner.write(b"device-name:laptop").unwrap();
    assert_eq!(host.read(&m3).unwrap(), b"device-name:laptop");
}

#[test]
fn pairing_with_wrong_psk_fails_at_third_message() {
    let jk = Keypair::generate();
    let hk = Keypair::generate();
    let mut joiner = Handshake::pairing(Role::Initiator, &jk, &derive_psk("ABCD-2345").unwrap()).unwrap();
    let mut host = Handshake::pairing(Role::Responder, &hk, &derive_psk("ABCD-2346").unwrap()).unwrap();

    let m1 = joiner.write(b"").unwrap();
    host.read(&m1).unwrap();
    let m2 = host.write(b"").unwrap();
    // 第二条（es 之后）还没混入 PSK，joiner 能读；psk3 在第三条才进入密钥。
    joiner.read(&m2).unwrap();
    let m3 = joiner.write(b"").unwrap();
    assert!(matches!(host.read(&m3), Err(Error::Decrypt)));
    // 失败后 host 握手作废，不能再用。
    assert!(!host.is_finished());
    assert!(host.into_transport().is_err());
}

#[test]
fn pairing_with_wrong_psk_cannot_decrypt_even_if_forced() {
    // 中间人（hub）不知道短码：用一个随便的 PSK 伪装成 host，也过不了。
    let jk = Keypair::generate();
    let mut joiner = Handshake::pairing(Role::Initiator, &jk, &derive_psk(CODE).unwrap()).unwrap();
    let mut mitm = Handshake::pairing(Role::Responder, &Keypair::generate(), &[0u8; 32]).unwrap();
    let m1 = joiner.write(b"").unwrap();
    mitm.read(&m1).unwrap();
    let m2 = mitm.write(b"").unwrap();
    joiner.read(&m2).unwrap();
    let m3 = joiner.write(b"").unwrap();
    assert!(mitm.read(&m3).is_err());
}

#[test]
fn handshake_state_machine_errors() {
    let (mut joiner, mut host, _, _) = pairing_pair(CODE);

    // 不该写的时候写。
    assert!(matches!(host.write(b""), Err(Error::NotMyTurn)));
    // 不该读的时候读。
    assert!(matches!(joiner.read(&[0u8; 32]), Err(Error::Noise(_))));
    // 没完成就 into_transport。
    joiner.write(b"").unwrap();
    assert!(matches!(joiner.write(b""), Err(Error::NotMyTurn)));
    assert!(matches!(joiner.into_transport(), Err(Error::NotFinished)));

    // 完成之后再读写。
    let (mut j2, mut h2, _, _) = pairing_pair(CODE);
    let m1b = j2.write(b"").unwrap();
    h2.read(&m1b).unwrap();
    let m2 = h2.write(b"").unwrap();
    j2.read(&m2).unwrap();
    let m3 = j2.write(b"").unwrap();
    h2.read(&m3).unwrap();
    assert!(matches!(j2.write(b""), Err(Error::Finished)));
    assert!(matches!(h2.read(&m3), Err(Error::Finished)));
}

#[test]
fn garbage_handshake_message_poisons_handshake() {
    let (_, mut host, _, _) = pairing_pair(CODE);
    assert!(host.read(&[1, 2, 3]).is_err());
    assert!(host.read(&[0u8; 32]).is_err()); // 之后连合法形状的消息也不再接受
    assert!(!host.is_my_turn());
    assert!(!host.is_finished());
}

#[test]
fn handshake_message_too_large_is_rejected() {
    let (mut joiner, _, _, _) = pairing_pair(CODE);
    assert!(matches!(joiner.write(&vec![0u8; 70_000]), Err(Error::Noise(_))));
    // payload 过大不会毁掉握手状态。
    assert!(joiner.write(b"").is_ok());
}

// ---------- 重连握手 ----------

#[test]
fn reconnect_handshake_success_and_remote_public_at_each_stage() {
    let client = Keypair::generate();
    let daemon = Keypair::generate();

    let mut ci = Handshake::reconnect(Role::Initiator, &client, Some(&daemon.public)).unwrap();
    let mut dr = Handshake::reconnect(Role::Responder, &daemon, None).unwrap();

    // IK initiator 一开始就知道对方公钥；responder 读完第一条才知道。
    assert_eq!(ci.remote_public(), Some(daemon.public));
    assert!(dr.remote_public().is_none());
    assert!(ci.is_my_turn() && !dr.is_my_turn());

    let m1 = ci.write(b"").unwrap();
    assert!(!ci.is_finished());
    dr.read(&m1).unwrap();
    assert_eq!(dr.remote_public(), Some(client.public));
    assert!(!dr.is_finished());
    assert!(dr.is_my_turn());

    let m2 = dr.write(b"").unwrap();
    assert!(dr.is_finished());
    ci.read(&m2).unwrap();
    assert!(ci.is_finished());

    let mut ct = ci.into_transport().unwrap();
    let mut dt = dr.into_transport().unwrap();
    assert_eq!(ct.remote_public(), daemon.public);
    assert_eq!(dt.remote_public(), client.public);

    let c = ct.encrypt(b"{\"jsonrpc\":\"2.0\"}").unwrap();
    assert_eq!(dt.decrypt(&c).unwrap(), b"{\"jsonrpc\":\"2.0\"}");
    let c = dt.encrypt(b"{\"result\":1}").unwrap();
    assert_eq!(ct.decrypt(&c).unwrap(), b"{\"result\":1}");
}

#[test]
fn reconnect_initiator_requires_remote_public() {
    let kp = Keypair::generate();
    assert!(matches!(Handshake::reconnect(Role::Initiator, &kp, None), Err(Error::InvalidKey)));
}

#[test]
fn reconnect_with_wrong_daemon_key_fails() {
    // 客户端以为在连 A，实际对面是 B：B 读不出第一条消息。
    let client = Keypair::generate();
    let a = Keypair::generate();
    let b = Keypair::generate();
    let mut ci = Handshake::reconnect(Role::Initiator, &client, Some(&a.public)).unwrap();
    let mut br = Handshake::reconnect(Role::Responder, &b, None).unwrap();
    let m1 = ci.write(b"").unwrap();
    assert!(matches!(br.read(&m1), Err(Error::Decrypt)));
}

#[test]
fn reconnect_responder_can_pin_initiator_key() {
    let client = Keypair::generate();
    let impostor = Keypair::generate();
    let daemon = Keypair::generate();

    // 期望的公钥对得上 → 通过。
    let mut ci = Handshake::reconnect(Role::Initiator, &client, Some(&daemon.public)).unwrap();
    let mut dr = Handshake::reconnect(Role::Responder, &daemon, Some(&client.public)).unwrap();
    dr.read(&ci.write(b"").unwrap()).unwrap();

    // 对不上 → InvalidKey，握手作废。
    let mut ii = Handshake::reconnect(Role::Initiator, &impostor, Some(&daemon.public)).unwrap();
    let mut dr = Handshake::reconnect(Role::Responder, &daemon, Some(&client.public)).unwrap();
    assert!(matches!(dr.read(&ii.write(b"").unwrap()), Err(Error::InvalidKey)));
    assert!(dr.write(b"").is_err());
    assert!(dr.into_transport().is_err());
}

#[test]
fn reconnect_is_fresh_each_time() {
    // 同一对静态密钥，两次重连的会话密钥不同（临时密钥参与）：A 会话的密文 B 会话解不开。
    let client = Keypair::generate();
    let daemon = Keypair::generate();
    let run = || {
        let mut ci = Handshake::reconnect(Role::Initiator, &client, Some(&daemon.public)).unwrap();
        let mut dr = Handshake::reconnect(Role::Responder, &daemon, None).unwrap();
        let m1 = ci.write(b"").unwrap();
        dr.read(&m1).unwrap();
        let m2 = dr.write(b"").unwrap();
        ci.read(&m2).unwrap();
        (ci.into_transport().unwrap(), dr.into_transport().unwrap())
    };
    let (mut c1, _d1) = run();
    let (_c2, mut d2) = run();
    let ct = c1.encrypt(b"x").unwrap();
    assert!(matches!(d2.decrypt(&ct), Err(Error::Decrypt)));
}

// ---------- 传输 ----------

#[test]
fn transport_small_message_framing() {
    let (mut a, mut b, _, _) = pair_transports();
    let ct = a.encrypt(b"hello").unwrap();
    // [u16 be 长度][密文 = 明文 + 16 字节 tag]
    assert_eq!(ct.len(), 2 + 5 + 16);
    assert_eq!(u16::from_be_bytes([ct[0], ct[1]]) as usize, 5 + 16);
    assert_eq!(b.decrypt(&ct).unwrap(), b"hello");
}

#[test]
fn transport_empty_plaintext_is_one_frame() {
    let (mut a, mut b, _, _) = pair_transports();
    let ct = a.encrypt(b"").unwrap();
    assert_eq!(ct.len(), 2 + 16);
    assert_eq!(u16::from_be_bytes([ct[0], ct[1]]), 16);
    assert_eq!(b.decrypt(&ct).unwrap(), Vec::<u8>::new());
    // 之后照常通信（计数器同步）。
    let ct = a.encrypt(b"after").unwrap();
    assert_eq!(b.decrypt(&ct).unwrap(), b"after");
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) ^ (i >> 8)) as u8).collect()
}

#[test]
fn transport_chunks_100_kib_roundtrip_both_directions() {
    let (mut a, mut b, _, _) = pair_transports();
    let msg = pattern(100 * 1024);
    let ct = a.encrypt(&msg).unwrap();
    // 100 KiB = 102400 = 65519 + 36881 → 两片。
    let first = u16::from_be_bytes([ct[0], ct[1]]) as usize;
    assert_eq!(first, MAX_CHUNK + 16);
    let second = u16::from_be_bytes([ct[2 + first], ct[3 + first]]) as usize;
    assert_eq!(second, msg.len() - MAX_CHUNK + 16);
    assert_eq!(ct.len(), 2 + first + 2 + second);
    assert_eq!(b.decrypt(&ct).unwrap(), msg);

    let back = pattern(250_000);
    let ct = b.encrypt(&back).unwrap();
    assert_eq!(a.decrypt(&ct).unwrap(), back);
}

#[test]
fn transport_chunk_boundaries() {
    let (mut a, mut b, _, _) = pair_transports();
    for len in [1, MAX_CHUNK - 1, MAX_CHUNK, MAX_CHUNK + 1, 2 * MAX_CHUNK, 2 * MAX_CHUNK + 1] {
        let msg = pattern(len);
        let ct = a.encrypt(&msg).unwrap();
        let frames = len.div_ceil(MAX_CHUNK);
        assert_eq!(ct.len(), len + frames * (2 + 16), "len={len}");
        assert_eq!(b.decrypt(&ct).unwrap(), msg, "len={len}");
    }
}

#[test]
fn transport_works_on_reconnect_session_too() {
    let (mut c, mut d, _, _) = reconnect_transports();
    let msg = pattern(MAX_CHUNK + 1000);
    assert_eq!(d.decrypt(&c.encrypt(&msg).unwrap()).unwrap(), msg);
    assert_eq!(c.decrypt(&d.encrypt(&msg).unwrap()).unwrap(), msg);
}

#[test]
fn transport_tampered_ciphertext_is_decrypt_error() {
    let (mut a, mut b, _, _) = pair_transports();
    let ct = a.encrypt(b"secret").unwrap();

    // 篡改密文体、篡改 tag 各一次。
    for idx in [2, ct.len() - 1] {
        let mut bad = ct.clone();
        bad[idx] ^= 0x01;
        assert!(matches!(b.decrypt(&bad), Err(Error::Decrypt)), "idx={idx}");
    }
    // 失败的 decrypt 没有副作用：原始密文仍然能解。
    assert_eq!(b.decrypt(&ct).unwrap(), b"secret");
}

#[test]
fn transport_tampered_second_frame_rolls_back_nonce() {
    let (mut a, mut b, _, _) = pair_transports();
    let msg = pattern(MAX_CHUNK + 10);
    let ct = a.encrypt(&msg).unwrap();
    let mut bad = ct.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xff; // 第二片的 tag
    assert!(matches!(b.decrypt(&bad), Err(Error::Decrypt)));
    // 第一片已被读过，但 nonce 回滚了，所以完整的原文仍能解出来。
    assert_eq!(b.decrypt(&ct).unwrap(), msg);
}

#[test]
fn transport_replay_and_reorder_fail() {
    let (mut a, mut b, _, _) = pair_transports();
    let c1 = a.encrypt(b"one").unwrap();
    let c2 = a.encrypt(b"two").unwrap();

    // 乱序：先给 c2。
    assert!(matches!(b.decrypt(&c2), Err(Error::Decrypt)));
    assert_eq!(b.decrypt(&c1).unwrap(), b"one");
    // 重放 c1。
    assert!(matches!(b.decrypt(&c1), Err(Error::Decrypt)));
    assert_eq!(b.decrypt(&c2).unwrap(), b"two");
}

#[test]
fn transport_wrong_direction_fails() {
    // 自己加密的东西自己解不了（两个方向的密钥不同）。
    let (mut a, _b, _, _) = pair_transports();
    let ct = a.encrypt(b"loop").unwrap();
    assert!(matches!(a.decrypt(&ct), Err(Error::Decrypt)));
}

#[test]
fn transport_malformed_frames() {
    let (mut a, mut b, _, _) = pair_transports();
    let ct = a.encrypt(b"hello world").unwrap();

    // 没有任何分片。
    assert!(matches!(b.decrypt(&[]), Err(Error::Malformed)));
    // 只有半个长度头。
    assert!(matches!(b.decrypt(&[0]), Err(Error::Malformed)));
    assert!(matches!(b.decrypt(&ct[..1]), Err(Error::Malformed)));
    // 声明的长度比实际内容长（截断）。
    assert!(matches!(b.decrypt(&ct[..ct.len() - 1]), Err(Error::Malformed)));
    assert!(matches!(b.decrypt(&ct[..2]), Err(Error::Malformed)));
    // 长度小于 tag。
    assert!(matches!(b.decrypt(&[0, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]), Err(Error::Malformed)));
    assert!(matches!(b.decrypt(&[0, 0]), Err(Error::Malformed)));
    // 第一片完整、第二片被截断：整体 Malformed，且不消耗 nonce。
    let mut two = ct.clone();
    two.extend_from_slice(&ct[..10]);
    assert!(matches!(b.decrypt(&two), Err(Error::Malformed)));
    // 尾部多余的零散字节。
    let mut trailing = ct.clone();
    trailing.push(0);
    assert!(matches!(b.decrypt(&trailing), Err(Error::Malformed)));
    // 以上都没动过 nonce，原始密文还能正常解。
    assert_eq!(b.decrypt(&ct).unwrap(), b"hello world");
}

#[test]
fn transport_many_messages_in_sequence() {
    let (mut a, mut b, _, _) = pair_transports();
    for i in 0..500u32 {
        let msg = pattern((i as usize * 37) % 5000);
        assert_eq!(b.decrypt(&a.encrypt(&msg).unwrap()).unwrap(), msg);
    }
}

#[test]
fn remote_public_after_transport_matches_known_peers() {
    let (c, d, client, daemon) = reconnect_transports();
    assert_eq!(c.remote_public(), daemon.public);
    assert_eq!(d.remote_public(), client.public);
}

#[test]
fn match_id_and_long_code() {
    let short = crate::generate_pair_code();
    assert_eq!(short.len(), 9);
    let long = crate::generate_pair_code_long();
    assert_eq!(long.len(), 19);
    let s1 = crate::derive_pair_secrets(&short).unwrap();
    let m1 = s1.match_id.clone();
    assert_eq!(m1.len(), 16);
    assert_eq!(crate::derive_match_id(&short.to_lowercase().replace('-', "")).unwrap(), m1);
    assert_ne!(m1, crate::hex(&s1.psk[..8]), "match id must not leak psk prefix");
    assert!(format!("{s1:?}").contains(&m1) && !format!("{s1:?}").contains(&crate::hex(&s1.psk)));
    assert!(crate::derive_pair_secrets(&long).is_ok());
    assert!(crate::derive_match_id("ABCD-EFG").is_err());
    assert!(crate::derive_match_id("0000-0000").is_err());
}
