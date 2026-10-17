//! 供 `tests/interop.mjs` 遥控的 Rust 对端：从 stdin 读一行命令，向 stdout 写一行结果。
//!
//! 它不含任何协议逻辑，只是把 `anycon_crypto` 的公开 API 一对一暴露成文本命令，
//! 这样 JS 里的 wasm 实现可以和**原生 Rust 实现**真实地跑完整握手 + 加解密，证明两边字节层面互通。
//!
//! 命令（空字节串用 `-` 表示；所有二进制都是小写 hex）：
//!   keypair [secret_hex]            → ok <secret> <public>        生成 / 还原本端静态密钥
//!   pairing <initiator|responder> <psk_hex>
//!   reconnect <initiator|responder> [remote_public_hex|-]
//!   turn | finished | remote        → ok true|false / ok <hex|none>
//!   write [payload_hex]             → ok <message_hex>
//!   read <message_hex>              → ok <payload_hex>
//!   transport                       → ok                          握手完成后换成传输通道
//!   encrypt <plaintext_hex>         → ok <ciphertext_hex>
//!   decrypt <ciphertext_hex>        → ok <plaintext_hex>
//!   tremote                         → ok <hex>                    Transport::remote_public
//!   psk <code>                      → ok <hex>
//!   fingerprint <public_hex>        → ok <fp>
//!   quit
//! 失败一律 `err <Error 文本>`。

use std::io::{BufRead, Write};

use anycon_crypto::{derive_psk, fingerprint, from_hex, hex, Error, Handshake, Keypair, Role, Transport};

fn bytes(s: Option<&str>) -> Result<Vec<u8>, Error> {
    match s {
        None | Some("-") => Ok(Vec::new()),
        Some(h) => from_hex(h),
    }
}

fn key32(s: Option<&str>) -> Result<[u8; 32], Error> {
    bytes(s)?.try_into().map_err(|_| Error::InvalidKey)
}

fn role(s: Option<&str>) -> Result<Role, Error> {
    match s {
        Some("initiator") => Ok(Role::Initiator),
        Some("responder") => Ok(Role::Responder),
        _ => Err(Error::Noise("bad role".into())),
    }
}

#[derive(Default)]
struct Peer {
    kp: Option<Keypair>,
    hs: Option<Handshake>,
    tr: Option<Transport>,
}

impl Peer {
    fn kp(&self) -> Result<&Keypair, Error> {
        self.kp.as_ref().ok_or_else(|| Error::Noise("no keypair".into()))
    }
    fn hs(&mut self) -> Result<&mut Handshake, Error> {
        self.hs.as_mut().ok_or_else(|| Error::Noise("no handshake".into()))
    }
    fn tr(&mut self) -> Result<&mut Transport, Error> {
        self.tr.as_mut().ok_or_else(|| Error::Noise("no transport".into()))
    }

    fn run(&mut self, line: &str) -> Result<String, Error> {
        let mut it = line.split_whitespace();
        let cmd = it.next().unwrap_or("");
        let a = it.next();
        let b = it.next();
        match cmd {
            "keypair" => {
                let kp = match a {
                    Some(s) => Keypair::from_secret(key32(Some(s))?),
                    None => Keypair::generate(),
                };
                let out = format!("{} {}", hex(&kp.secret), hex(&kp.public));
                self.kp = Some(kp);
                Ok(out)
            }
            "pairing" => {
                let hs = Handshake::pairing(role(a)?, self.kp()?, &key32(b)?)?;
                self.hs = Some(hs);
                Ok(String::new())
            }
            "reconnect" => {
                let remote = match b {
                    None | Some("-") => None,
                    Some(h) => Some(key32(Some(h))?),
                };
                let hs = Handshake::reconnect(role(a)?, self.kp()?, remote.as_ref())?;
                self.hs = Some(hs);
                Ok(String::new())
            }
            "turn" => Ok(self.hs()?.is_my_turn().to_string()),
            "finished" => Ok(self.hs()?.is_finished().to_string()),
            "remote" => Ok(self.hs()?.remote_public().map(|k| hex(&k)).unwrap_or_else(|| "none".into())),
            "write" => Ok(hex(&self.hs()?.write(&bytes(a)?)?)),
            "read" => {
                let p = self.hs()?.read(&bytes(a)?)?;
                Ok(if p.is_empty() { "-".into() } else { hex(&p) })
            }
            "transport" => {
                let hs = self.hs.take().ok_or_else(|| Error::Noise("no handshake".into()))?;
                self.tr = Some(hs.into_transport()?);
                Ok(String::new())
            }
            "encrypt" => Ok(hex(&self.tr()?.encrypt(&bytes(a)?)?)),
            "decrypt" => {
                let p = self.tr()?.decrypt(&bytes(a)?)?;
                Ok(if p.is_empty() { "-".into() } else { hex(&p) })
            }
            "tremote" => Ok(hex(&self.tr()?.remote_public())),
            "psk" => Ok(hex(&derive_psk(a.unwrap_or(""))?)),
            "fingerprint" => Ok(fingerprint(&key32(a)?)),
            other => Err(Error::Noise(format!("unknown command {other:?}"))),
        }
    }
}

fn main() {
    let mut peer = Peer::default();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let line = line.expect("read stdin");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "quit" {
            break;
        }
        let reply = match peer.run(line) {
            Ok(s) if s.is_empty() => "ok".to_string(),
            Ok(s) => format!("ok {s}"),
            Err(e) => format!("err {e}"),
        };
        writeln!(out, "{reply}").expect("write stdout");
        out.flush().expect("flush stdout");
    }
}
