//! Zenoh user/password credentials: the per-service credentials file and
//! the router's dictionary.
//!
//! # The credentials file (`MW_ZENOH_CREDENTIALS`)
//!
//! A service reads its Zenoh credentials from the file named by
//! `MW_ZENOH_CREDENTIALS`. In the cluster that file is key
//! [`SECRET_KEY`] of the Secret [`secret_name`]`(user)` (`zenoh-<user>`),
//! mounted at [`MOUNT_PATH`]. Its format is exactly two lines:
//!
//! ```text
//! <user>
//! <password>
//! ```
//!
//! each ended by `\n` (the last newline may be missing). Nothing else is
//! accepted: no blank lines, no `\r`, no surrounding whitespace, no third
//! line ([`Credentials::parse`]). [`Credentials::render`] writes the same
//! form, so `parse(render(c)) == c`.
//!
//! * The user is the principal's Zenoh username: an RFC 1123 label (the
//!   Kubernetes name of a service, `robot-<name>` for a robot), so it never
//!   contains the dictionary's `:` separator.
//! * The password is [`MIN_PASSWORD_LEN`]..=[`MAX_PASSWORD_LEN`] bytes of
//!   visible ASCII (`!`..=`~`), so it survives Zenoh's dictionary parser,
//!   which trims whitespace. Passwords are generated outside this crate
//!   (by the ops CLI or the e2e) and never committed; [`Password`]'s
//!   `Debug` does not print it and no error echoes it.
//!
//! # The router's dictionary
//!
//! [`dictionary`] renders the `dictionary_file` zenohd reads for
//! `transport/auth/usrpwd`: one `<user>:<password>` line per principal,
//! sorted by user. It is key [`DICTIONARY_KEY`] of Secret
//! [`DICTIONARY_SECRET`], mounted in the router at
//! [`DICTIONARY_PATH`](super::DICTIONARY_PATH).

use std::fmt;
use std::path::Path;

use middleware::grpc::istio::is_dns_label;

/// Where a service pod mounts its credentials Secret (the value of
/// `MW_ZENOH_CREDENTIALS` in the rendered manifests).
pub const MOUNT_PATH: &str = "/var/run/mw/zenoh/credentials";

/// The key of the credentials file inside a service's Secret.
pub const SECRET_KEY: &str = "credentials";

/// The Secret holding the router's dictionary.
pub const DICTIONARY_SECRET: &str = "zenohd-auth";

/// The key of the dictionary file inside [`DICTIONARY_SECRET`].
pub const DICTIONARY_KEY: &str = "dictionary";

/// Shortest password accepted.
pub const MIN_PASSWORD_LEN: usize = 16;

/// Longest password accepted.
pub const MAX_PASSWORD_LEN: usize = 256;

/// The Secret holding `user`'s credentials file: `zenoh-<user>`.
pub fn secret_name(user: &str) -> String {
    format!("zenoh-{user}")
}

/// A Zenoh password: [`MIN_PASSWORD_LEN`]..=[`MAX_PASSWORD_LEN`] bytes of
/// visible ASCII. `Debug` prints `Password(***)`.
#[derive(Clone, PartialEq, Eq)]
pub struct Password(String);

impl Password {
    /// Checks `text` against the module rules. The error never contains
    /// the text.
    pub fn new(text: impl Into<String>) -> Result<Self, CredentialsError> {
        let text = text.into();
        let len_ok = (MIN_PASSWORD_LEN..=MAX_PASSWORD_LEN).contains(&text.len());
        if len_ok && text.bytes().all(|b| b.is_ascii_graphic()) {
            Ok(Password(text))
        } else {
            Err(CredentialsError::InvalidPassword)
        }
    }

    /// The password itself.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(***)")
    }
}

/// One principal's Zenoh username and password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    user: String,
    password: Password,
}

impl Credentials {
    /// Credentials for `user` (an RFC 1123 label) with `password`.
    pub fn new(user: impl Into<String>, password: Password) -> Result<Self, CredentialsError> {
        let user = user.into();
        if is_dns_label(&user) {
            Ok(Credentials { user, password })
        } else {
            Err(CredentialsError::InvalidUser(user))
        }
    }

    /// The username.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// The password.
    pub fn password(&self) -> &Password {
        &self.password
    }

    /// Parses a credentials file (module docs). Pure.
    pub fn parse(text: &str) -> Result<Self, CredentialsError> {
        let body = text.strip_suffix('\n').unwrap_or(text);
        let mut lines = body.split('\n');
        let (Some(user), Some(password), None) = (lines.next(), lines.next(), lines.next()) else {
            return Err(CredentialsError::Format);
        };
        Credentials::new(user, Password::new(password)?)
    }

    /// Reads and parses the credentials file at `path` (the I/O shell
    /// around [`Credentials::parse`]).
    pub fn from_file(path: &Path) -> Result<Self, CredentialsError> {
        let text = std::fs::read_to_string(path).map_err(|e| CredentialsError::Io(e.kind()))?;
        Credentials::parse(&text)
    }

    /// The credentials file: `<user>\n<password>\n`.
    pub fn render(&self) -> String {
        format!("{}\n{}\n", self.user, self.password.0)
    }
}

/// The router's dictionary file: one `<user>:<password>` line per entry,
/// sorted by user. Two entries for one user are an error. Pure.
pub fn dictionary(credentials: &[Credentials]) -> Result<String, CredentialsError> {
    let mut sorted: Vec<&Credentials> = credentials.iter().collect();
    sorted.sort_by(|a, b| a.user.cmp(&b.user));
    if let Some(w) = sorted.windows(2).find(|w| w[0].user == w[1].user) {
        return Err(CredentialsError::DuplicateUser(w[0].user.clone()));
    }
    Ok(sorted
        .iter()
        .map(|c| format!("{}:{}\n", c.user, c.password.0))
        .collect())
}

/// Why credentials were refused. No variant carries a password.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CredentialsError {
    /// The file is not exactly two `\n`-terminated lines.
    Format,
    /// The username is not an RFC 1123 label.
    InvalidUser(String),
    /// The password breaks the length or character rule.
    InvalidPassword,
    /// Two entries name the same user.
    DuplicateUser(String),
    /// Reading the file failed.
    Io(std::io::ErrorKind),
}

impl fmt::Display for CredentialsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialsError::Format => {
                f.write_str("credentials file must be two lines: user, then password")
            }
            CredentialsError::InvalidUser(u) => {
                write!(f, "zenoh user {u:?} is not an RFC 1123 label")
            }
            CredentialsError::InvalidPassword => write!(
                f,
                "zenoh password must be {MIN_PASSWORD_LEN}..={MAX_PASSWORD_LEN} bytes of visible ASCII"
            ),
            CredentialsError::DuplicateUser(u) => write!(f, "zenoh user {u:?} given twice"),
            CredentialsError::Io(kind) => write!(f, "reading the credentials file: {kind}"),
        }
    }
}

impl std::error::Error for CredentialsError {}

#[cfg(test)]
mod tests {
    use super::*;

    const PW: &str = "s3cret-pass-0123456789";

    fn creds(user: &str) -> Credentials {
        Credentials::new(user, Password::new(PW).unwrap()).unwrap()
    }

    #[test]
    fn z20_credentials_file_round_trips_and_is_strict() {
        let c = creds("demo-echo");
        assert_eq!(c.render(), format!("demo-echo\n{PW}\n"));
        assert_eq!(Credentials::parse(&c.render()), Ok(c.clone()));
        assert_eq!(
            Credentials::parse(&format!("demo-echo\n{PW}")),
            Ok(c),
            "final newline optional"
        );
        for bad in [
            String::new(),
            "demo-echo\n".to_owned(),
            format!("demo-echo\n{PW}\n\n"),
            format!("demo-echo\n{PW}\nextra\n"),
            format!("demo-echo\r\n{PW}\r\n"),
            format!(" demo-echo\n{PW}\n"),
        ] {
            assert!(Credentials::parse(&bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            Credentials::parse(&format!("demo:echo\n{PW}\n")),
            Err(CredentialsError::InvalidUser("demo:echo".into()))
        );
        assert_eq!(
            Credentials::parse("demo-echo\nshort\n"),
            Err(CredentialsError::InvalidPassword)
        );
        assert_eq!(
            Credentials::parse(&format!("demo-echo\n{PW} x\n")),
            Err(CredentialsError::InvalidPassword),
            "no whitespace inside"
        );
    }

    #[test]
    fn z20_passwords_never_print() {
        let c = creds("demo-echo");
        assert!(!format!("{c:?}").contains(PW));
        let e = Password::new("x".repeat(MAX_PASSWORD_LEN + 1)).unwrap_err();
        assert_eq!(e, CredentialsError::InvalidPassword);
        assert!(Password::new("a".repeat(MAX_PASSWORD_LEN)).is_ok());
        assert!(Password::new("a".repeat(MIN_PASSWORD_LEN - 1)).is_err());
    }

    #[test]
    fn z20_dictionary_is_sorted_and_refuses_duplicates() {
        let d = dictionary(&[creds("zz"), creds("demo-echo")]).unwrap();
        assert_eq!(d, format!("demo-echo:{PW}\nzz:{PW}\n"));
        assert_eq!(dictionary(&[]).unwrap(), "");
        assert_eq!(
            dictionary(&[creds("a"), creds("a")]),
            Err(CredentialsError::DuplicateUser("a".into()))
        );
        assert_eq!(secret_name("demo-echo"), "zenoh-demo-echo");
    }

    #[test]
    fn z20_from_file_reads_and_reports_io() {
        let dir = std::env::temp_dir().join(format!("mw-zenoh-cred-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials");
        std::fs::write(&path, creds("demo-echo").render()).unwrap();
        assert_eq!(Credentials::from_file(&path), Ok(creds("demo-echo")));
        assert_eq!(
            Credentials::from_file(&dir.join("missing")),
            Err(CredentialsError::Io(std::io::ErrorKind::NotFound))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
