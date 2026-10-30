//! The site's YouTube cookie, for trusted workers. It is sent encrypted to a
//! fresh X25519 key per request and lives only in memory here (wiped when
//! dropped). Each yt-dlp run gets its own short-lived copy on disk.
//!
//!   POST /api/worker/v1/cookie {"public": b64(x25519 public key)}
//!     -> {version, server_public, nonce, ciphertext} (base64) or {version: null}
//!   key   = HKDF-SHA256(ikm = X25519(mine, server_public), salt = nonce, info = INFO)
//!   plain = AES-256-GCM(key, nonce, ciphertext, aad = INFO)  (a Netscape cookies.txt)

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use std::io::Write;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const INFO: &[u8] = b"manjikick worker cookie v1";

pub struct SiteCookie {
    pub version: String,
    text: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for SiteCookie {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SiteCookie {{ version: {:?}, .. }}", self.version)
    }
}

pub enum Fetched {
    Cookie(SiteCookie),
    /// No cookie set on the site, or this worker isn't trusted.
    None,
}

/// Asks the site for its cookie. Errors are network or format problems.
pub fn fetch(api: &crate::api::Api) -> Result<Fetched, String> {
    let secret = x25519_dalek::EphemeralSecret::random();
    let public = x25519_dalek::PublicKey::from(&secret);
    let reply = match api.cookie(&B64.encode(public.as_bytes())) {
        Ok(v) => v,
        Err(crate::api::ApiError::Auth(403)) => return Ok(Fetched::None),
        Err(e) => return Err(e.to_string()),
    };
    open(secret, &reply)
}

/// Decrypts a /cookie reply with the secret whose public key was sent.
fn open(secret: x25519_dalek::EphemeralSecret, reply: &serde_json::Value) -> Result<Fetched, String> {
    let Some(server) = server_key(reply)? else { return Ok(Fetched::None) };
    let shared = secret.diffie_hellman(&x25519_dalek::PublicKey::from(server));
    open_shared(shared.as_bytes(), reply)
}

fn field(reply: &serde_json::Value, k: &str) -> Result<Vec<u8>, String> {
    B64.decode(reply[k].as_str().ok_or_else(|| format!("cookie reply has no {k}"))?)
        .map_err(|_| format!("cookie reply: bad {k}"))
}

/// The server's one-off public key, or None when the reply says "no cookie".
fn server_key(reply: &serde_json::Value) -> Result<Option<[u8; 32]>, String> {
    if reply["version"].as_str().is_none() {
        return Ok(None);
    }
    let key: [u8; 32] = field(reply, "server_public")?.try_into().map_err(|_| "cookie reply: bad key length")?;
    Ok(Some(key))
}

fn open_shared(shared: &[u8; 32], reply: &serde_json::Value) -> Result<Fetched, String> {
    let Some(version) = reply["version"].as_str().map(str::to_string) else { return Ok(Fetched::None) };
    let nonce = field(reply, "nonce")?;
    if nonce.len() != 12 {
        return Err("cookie reply: bad nonce length".into());
    }
    let ciphertext = field(reply, "ciphertext")?;
    let mut key = Zeroizing::new([0u8; 32]);
    hkdf::Hkdf::<sha2::Sha256>::new(Some(&nonce), shared)
        .expand(INFO, key.as_mut())
        .map_err(|_| "cookie: key derivation failed")?;
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).map_err(|_| "cookie: bad key")?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ciphertext, aad: INFO })
        .map_err(|_| "cookie: could not decrypt (wrong key or damaged)")?;
    Ok(Fetched::Cookie(SiteCookie { version, text: Zeroizing::new(plain) }))
}

/// A cookies.txt for one yt-dlp run; deleted when dropped.
pub struct TempCookieFile(PathBuf);

impl TempCookieFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempCookieFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes the cookie to a fresh random-named file in `folder` (only this
/// user can read it: Windows marks it temporary inside the per-user data
/// folder; Unix creates it 0600).
pub fn write_temp(cookie: &SiteCookie, folder: &Path) -> std::io::Result<TempCookieFile> {
    let mut rnd = [0u8; 12];
    getrandom::getrandom(&mut rnd).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let name: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let path = folder.join(format!("c{name}.txt"));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.attributes(0x100); // FILE_ATTRIBUTE_TEMPORARY
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let guard = TempCookieFile(path.clone());
    let mut f = opts.open(&path)?;
    f.write_all(&cookie.text)?;
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(f: Fetched) -> Vec<u8> {
        match f {
            Fetched::Cookie(c) => c.text.to_vec(),
            Fetched::None => panic!("no cookie"),
        }
    }

    /// Seals like the site does (worker_cookie.seal_for) and opens it again.
    #[test]
    fn round_trip() {
        let worker = x25519_dalek::EphemeralSecret::random();
        let worker_public = x25519_dalek::PublicKey::from(&worker);
        let server = x25519_dalek::EphemeralSecret::random();
        let server_public = x25519_dalek::PublicKey::from(&server);
        let shared = server.diffie_hellman(&worker_public);
        let nonce = [7u8; 12];
        let mut key = [0u8; 32];
        hkdf::Hkdf::<sha2::Sha256>::new(Some(&nonce), shared.as_bytes()).expand(INFO, &mut key).unwrap();
        let plain = b"# Netscape HTTP Cookie File\n.youtube.com\tTRUE\t/\tTRUE\t0\tX\tY\n";
        let ct = Aes256Gcm::new_from_slice(&key).unwrap()
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: INFO }).unwrap();
        let reply = serde_json::json!({"ok": true, "version": "v1", "server_public": B64.encode(server_public.as_bytes()),
            "nonce": B64.encode(nonce), "ciphertext": B64.encode(&ct)});
        assert_eq!(text(open(worker, &reply).unwrap()), plain.to_vec());
        assert!(matches!(open(x25519_dalek::EphemeralSecret::random(), &reply), Err(_)));
        assert!(matches!(open(x25519_dalek::EphemeralSecret::random(), &serde_json::json!({"version": null})), Ok(Fetched::None)));
    }

    #[test]
    fn temp_file_goes_away() {
        let dir = std::env::temp_dir().join(format!("mkw-cookie-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let c = SiteCookie { version: "v".into(), text: Zeroizing::new(b"x".to_vec()) };
        let f = write_temp(&c, &dir).unwrap();
        let p = f.path().to_path_buf();
        assert_eq!(std::fs::read(&p).unwrap(), b"x");
        drop(f);
        assert!(!p.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A reply made by the site's own Python code: set MKW_COOKIE_TEST_REPLY
    /// to JSON with the reply fields plus "worker_secret" (base64).
    #[test]
    fn opens_a_reply_from_the_site_code() {
        let Ok(raw) = std::env::var("MKW_COOKIE_TEST_REPLY") else { return };
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let secret: [u8; 32] = B64.decode(v["worker_secret"].as_str().unwrap()).unwrap().try_into().unwrap();
        let server = server_key(&v).unwrap().unwrap();
        let shared = x25519_dalek::x25519(secret, server);
        assert_eq!(String::from_utf8(text(open_shared(&shared, &v).unwrap())).unwrap(), v["expect"].as_str().unwrap());
    }
}
