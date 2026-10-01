//! Machine identity and API-key derivation.
//!
//! Getting any byte wrong here changes the API keys every client already has,
//! so the derivation is reproduced exactly rather than "cleaned up".

use std::path::Path;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::error::{DbError, DbResult};
use crate::paths::Paths;

pub const MACHINE_ID_SALT: &str = "endpoint-proxy-salt";
pub const CLI_AUTH_SALT: &str = "9r-cli-auth";
pub const API_KEY_SECRET_DEFAULT: &str = "endpoint-proxy-api-key-secret";

/// `sha256(raw + saltValue + extra).hex[..16]`, where `extra` is the CLI secret
/// **only** for the CLI salt. Both files are created on first use with mode
/// 0600.
pub fn consistent_machine_id(paths: &Paths, salt: Option<&str>) -> DbResult<String> {
    let raw = load_raw_machine_id(paths)?;
    // An explicit salt wins over `MACHINE_ID_SALT`, which wins over the
    // built-in default.
    let env_salt = std::env::var("MACHINE_ID_SALT").ok();
    let salt_value = salt
        .or(env_salt.as_deref().filter(|s| !s.is_empty()))
        .unwrap_or(MACHINE_ID_SALT);
    let mut input = String::with_capacity(raw.len() + salt_value.len() + 64);
    input.push_str(&raw);
    input.push_str(salt_value);
    // The comparison is on the *resolved* salt, so a `MACHINE_ID_SALT` env var
    // equal to the CLI salt also pulls in the CLI secret.
    if salt_value == CLI_AUTH_SALT {
        input.push_str(&load_cli_secret(paths)?);
    }
    Ok(hex_sha256(&input)[..16].to_string())
}

/// The persisted raw id, else the OS machine id, else a random UUID, always
/// persisted.
pub fn raw_machine_id(paths: &Paths) -> DbResult<String> {
    load_raw_machine_id(paths)
}

fn load_raw_machine_id(paths: &Paths) -> DbResult<String> {
    if let Ok(existing) = std::fs::read_to_string(&paths.machine_id_file) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let value = os_machine_id().unwrap_or_else(random_uuid);
    write_secret_file(&paths.machine_id_file, &value)?;
    Ok(value)
}

fn load_cli_secret(paths: &Paths) -> DbResult<String> {
    if let Ok(existing) = std::fs::read_to_string(&paths.cli_secret_file) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let value = random_hex(32);
    write_secret_file(&paths.cli_secret_file, &value)?;
    Ok(value)
}

/// The CLI secret, for callers that need to persist or compare it.
/// `JWT_SECRET` wins and is used verbatim. Otherwise the file, trimmed, is used
/// when it is non-empty; an empty or whitespace-only file is treated as absent
/// and regenerated, so a truncated file cannot leave every session signed with
/// an empty key. The file is created on first use: 32 random bytes, hex, mode
/// 0600.
pub fn jwt_secret(paths: &Paths) -> DbResult<String> {
    if let Ok(v) = std::env::var("JWT_SECRET")
        && !v.is_empty()
    {
        return Ok(v);
    }
    if let Ok(existing) = std::fs::read_to_string(&paths.jwt_secret_file) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let value = random_hex(32);
    write_secret_file(&paths.jwt_secret_file, &value)?;
    Ok(value)
}

/// Raw OS machine id, reproducing `node-machine-id`'s per-platform command.
///
/// The npm package shells out; so does this. A native API would be cleaner but
/// would produce a *different* string on some platforms, which changes the
/// derived machine id.
fn os_machine_id() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        windows_machine_guid()
    }
    #[cfg(target_os = "macos")]
    {
        ioreg_platform_uuid()
    }
    #[cfg(target_os = "linux")]
    {
        linux_machine_id()
    }
    #[cfg(target_os = "freebsd")]
    {
        freebsd_machine_id()
    }
    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "linux",
        target_os = "freebsd"
    )))]
    {
        None
    }
}

#[cfg(target_os = "windows")]
fn windows_machine_guid() -> Option<String> {
    // node-machine-id runs REG.exe directly, or through sysnative when a 32-bit
    // process is running on 64-bit Windows (WOW64 registry redirection would
    // otherwise hide the key).
    let out = if cfg!(target_arch = "x86") && std::env::var_os("PROCESSOR_ARCHITEW6432").is_some() {
        let sysnative = std::env::var("windir").ok()?;
        run_command(
            &format!("{sysnative}\\sysnative\\cmd.exe"),
            &[
                "/c",
                "%windir%\\System32\\REG.exe",
                "QUERY",
                "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography",
                "/v",
                "MachineGuid",
            ],
        )?
    } else {
        let windir = std::env::var("windir").ok()?;
        run_command(
            &format!("{windir}\\System32\\REG.exe"),
            &[
                "QUERY",
                "HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography",
                "/v",
                "MachineGuid",
            ],
        )?
    };
    // Split on REG_SZ and take the value; lowercase, strip whitespace.
    let after = out.split("REG_SZ").nth(1)?;
    Some(after.trim().to_lowercase())
}

#[cfg(target_os = "macos")]
fn ioreg_platform_uuid() -> Option<String> {
    let out = run_command("ioreg", &["-rd1", "-c", "IOPlatformExpertDevice"])?;
    let after = out.split("IOPlatformUUID").nth(1)?;
    // node-machine-id strips `=`, whitespace and quotes.
    let cleaned: String = after
        .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
        .trim_matches('"')
        .trim()
        .to_lowercase();
    Some(cleaned)
}

#[cfg(target_os = "linux")]
fn linux_machine_id() -> Option<String> {
    // `( cat /var/lib/dbus/machine-id /etc/machine-id 2> /dev/null || hostname ) | head -n 1`
    for path in ["/var/lib/dbus/machine-id", "/etc/machine-id"] {
        if let Ok(s) = std::fs::read_to_string(path) {
            let first = s.lines().next().unwrap_or("").trim().to_lowercase();
            if !first.is_empty() {
                return Some(first);
            }
        }
    }
    let out = run_command("hostname", &[])?;
    Some(out.trim().to_lowercase())
}

#[cfg(target_os = "freebsd")]
fn freebsd_machine_id() -> Option<String> {
    let out = run_command("kenv", &["-q", "smbios.system.uuid"])
        .or_else(|| run_command("sysctl", &["-n", "kern.hostuuid"]))?;
    Some(out.trim().to_lowercase())
}

/// Run a command and return stdout, or `None` on any failure — the npm package
/// swallows errors the same way.
#[cfg(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd"
))]
fn run_command(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn random_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex_encode(&buf)
}

fn hex_sha256(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex_encode(&hasher.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Write a file with mode 0600 where the platform supports it.
fn write_secret_file(path: &Path, contents: &str) -> DbResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DbError::io(parent, e))?;
    }
    std::fs::write(path, contents).map_err(|e| DbError::io(path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(path, perms);
    }
    Ok(())
}

// ─── API keys ────────────────────────────────────────────────────────────

/// 6 characters from a fixed 36-char alphabet.
pub fn generate_key_id() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..6)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// HMAC-SHA256(secret, machineId + keyId), hex, first 8 characters.
pub fn generate_crc(secret: &str, machine_id: &str, key_id: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(machine_id.as_bytes());
    mac.update(key_id.as_bytes());
    hex_encode(&mac.finalize().into_bytes())[..8].to_string()
}

/// The `sk-{machineId}-{keyId}-{crc}` key shape.
pub fn generate_api_key_with_machine(secret: &str, machine_id: &str) -> String {
    let key_id = generate_key_id();
    let crc = generate_crc(secret, machine_id, &key_id);
    format!("sk-{machine_id}-{key_id}-{crc}")
}

/// The API key secret, with the built-in default when unset or empty.
pub fn api_key_secret() -> String {
    std::env::var("API_KEY_SECRET")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| API_KEY_SECRET_DEFAULT.to_string())
}

/// The 4-part format with a valid CRC yields
/// `machine_id = Some(..), is_new_format = true`; the legacy `sk-{random8}`
/// 2-part form yields `machine_id = None, is_new_format = false`. Anything else
/// is `None`.
pub fn parse_api_key(secret: &str, key: &str) -> Option<ApiKeyParts> {
    if !key.starts_with("sk-") {
        return None;
    }
    let parts: Vec<&str> = key.split('-').collect();
    if parts.len() == 4 {
        let (machine_id, key_id, crc) = (parts[1], parts[2], parts[3]);
        if !verify_api_key_crc(secret, machine_id, key_id, crc) {
            return None;
        }
        return Some(ApiKeyParts {
            machine_id: Some(machine_id.to_string()),
            key_id: key_id.to_string(),
            is_new_format: true,
        });
    }
    if parts.len() == 2 {
        return Some(ApiKeyParts {
            machine_id: None,
            key_id: parts[1].to_string(),
            is_new_format: false,
        });
    }
    None
}

pub fn verify_api_key_crc(secret: &str, machine_id: &str, key_id: &str, crc: &str) -> bool {
    let expected = generate_crc(secret, machine_id, key_id);
    // Constant-time compare: the CRC is a truncated MAC, so a length-aware
    // equality check is the right primitive.
    use subtle::ConstantTimeEq;
    expected.as_bytes().ct_eq(crc.as_bytes()).into()
}

/// 4 dash-separated parts with the `sk` prefix.
pub fn is_new_format_key(key: &str) -> bool {
    let parts: Vec<&str> = key.split('-').collect();
    parts.len() == 4 && parts[0] == "sk"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyParts {
    /// `null` for a legacy 2-part key.
    pub machine_id: Option<String>,
    pub key_id: String,
    pub is_new_format: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_round_trips_through_parse() {
        let secret = "test-secret";
        let key = generate_api_key_with_machine(secret, "abc123");
        assert!(is_new_format_key(&key));
        let parts = parse_api_key(secret, &key).expect("valid crc");
        assert_eq!(parts.machine_id.as_deref(), Some("abc123"));
        assert!(parts.is_new_format);
        assert_eq!(key.split('-').count(), 4);
    }

    #[test]
    fn tampered_crc_is_rejected() {
        let secret = "test-secret";
        let key = generate_api_key_with_machine(secret, "abc123");
        let mut chars: Vec<char> = key.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        assert!(parse_api_key(secret, &tampered).is_none());
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let key = generate_api_key_with_machine("secret-a", "abc123");
        assert!(parse_api_key("secret-b", &key).is_none());
    }

    #[test]
    fn legacy_two_part_key_parses_as_old_format() {
        assert!(!is_new_format_key("sk-abcd1234"));
        let parts = parse_api_key("s", "sk-abcd1234").expect("legacy key is accepted");
        assert_eq!(parts.machine_id, None);
        assert_eq!(parts.key_id, "abcd1234");
        assert!(!parts.is_new_format);
    }

    #[test]
    fn keys_without_the_sk_prefix_or_wrong_shape_are_rejected() {
        assert!(parse_api_key("s", "abcd1234").is_none());
        assert!(parse_api_key("s", "sk-a-b").is_none());
        assert!(parse_api_key("s", "").is_none());
    }

    #[test]
    fn key_id_uses_the_reference_alphabet() {
        for _ in 0..200 {
            let id = generate_key_id();
            assert_eq!(id.len(), 6);
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            );
        }
    }

    #[test]
    fn crc_is_deterministic_and_eight_hex_chars() {
        let a = generate_crc("s", "m", "k");
        let b = generate_crc("s", "m", "k");
        assert_eq!(a, b);
        assert_eq!(a.len(), 8);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn machine_id_is_stable_across_calls_and_sixteen_chars() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        let a = consistent_machine_id(&paths, None).unwrap();
        let b = consistent_machine_id(&paths, None).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn cli_salt_changes_the_machine_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        let plain = consistent_machine_id(&paths, None).unwrap();
        let cli = consistent_machine_id(&paths, Some(CLI_AUTH_SALT)).unwrap();
        assert_ne!(plain, cli);
        assert_eq!(cli.len(), 16);
    }

    #[test]
    fn cli_machine_id_depends_on_the_secret_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        let first = consistent_machine_id(&paths, Some(CLI_AUTH_SALT)).unwrap();

        std::fs::remove_file(&paths.cli_secret_file).unwrap();
        let second = consistent_machine_id(&paths, Some(CLI_AUTH_SALT)).unwrap();
        assert_ne!(first, second, "regenerating cli-secret must change the id");
    }

    #[test]
    fn custom_salt_matches_the_formula() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        let raw = raw_machine_id(&paths).unwrap();
        let expected = hex_sha256(&format!("{raw}my-salt"))[..16].to_string();
        assert_eq!(
            consistent_machine_id(&paths, Some("my-salt")).unwrap(),
            expected
        );
    }

    #[test]
    fn jwt_secret_is_persisted_and_stable() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        let first = jwt_secret(&paths).unwrap();
        assert_eq!(first.len(), 64, "32 random bytes, hex");
        assert_eq!(jwt_secret(&paths).unwrap(), first);
        assert!(paths.jwt_secret_file.exists());
    }

    #[test]
    fn empty_jwt_secret_file_is_regenerated() {
        // A whitespace-only file trims to "" — an empty signing key. It is
        // treated as absent and replaced with a real secret rather than kept.
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::new(dir.path().to_path_buf());
        std::fs::write(&paths.jwt_secret_file, "  \n").unwrap();
        let secret = jwt_secret(&paths).unwrap();
        assert_eq!(secret.len(), 64, "regenerated, not the empty string");
        assert_eq!(
            std::fs::read_to_string(&paths.jwt_secret_file).unwrap(),
            secret
        );
    }
}
