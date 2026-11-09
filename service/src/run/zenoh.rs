//! The runner's Zenoh session (M10, runner side).
//!
//! A service whose spec publishes or subscribes
//! ([`ServiceSpec::uses_zenoh`]) opens one Zenoh session in **client**
//! mode to `MW_ZENOH_ROUTER` (a locator such as
//! `tcp/zenohd.apps.svc.cluster.local:7447`; there is no default, and a
//! spec that uses Zenoh without one fails to start with
//! [`ServiceError::NoZenohRouter`]). Multicast scouting is off; the
//! connection attempt is bounded by [`CONNECT_TIMEOUT_MS`] and the run
//! fails if it does not succeed (the pod restarts and tries again).
//!
//! # Credentials
//!
//! With `MW_ZENOH_CREDENTIALS` set, the session authenticates with
//! Zenoh's user/password scheme (`transport/auth/usrpwd`). The file holds
//! **one line `<user>:<password>`**: the format of one entry of zenohd's
//! own `usrpwd` dictionary file, so the router's dictionary is the
//! concatenation of the services' files. The user is the text before the
//! first `:`, the password everything after it, both trimmed and
//! non-empty; blank lines are ignored and a second entry is an error.
//! Errors never quote the file ([`parse_credentials`]), and
//! [`Credentials`]' `Debug` hides the password.
//!
//! # Publishing and subscribing
//!
//! Handlers publish through [`Ctx::publisher`](crate::Ctx::publisher),
//! which refuses keys outside the spec's `publishes`. Each
//! [`on_sample`](crate::ServiceBuilder::on_sample) hook gets one task that
//! declares a subscriber for its filter and calls the hook with every
//! sample in order ([`run_hook`]); `subscribes` filters without a hook are
//! only declared (manifests, router ACL).

use std::path::Path;

use middleware::transport::{Subscribe, Subscriber};
use middleware::zenoh::{ZenohError, ZenohTransport};

use crate::builder::SampleHook;
use crate::config::Config;
use crate::error::ServiceError;
use crate::spec::{ServiceSpec, parse_filter};

/// Bound on connecting to the router at start, in milliseconds.
pub const CONNECT_TIMEOUT_MS: u32 = 10_000;

/// The largest sample a hook receives; bigger ones are dropped with a
/// warning.
pub const SAMPLE_BUFFER: usize = 64 * 1024;

/// A Zenoh user and password (module docs).
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// The user name.
    pub user: String,
    password: String,
}

impl Credentials {
    /// `user` with `password`.
    pub fn new(user: impl Into<String>, password: impl Into<String>) -> Self {
        Credentials {
            user: user.into(),
            password: password.into(),
        }
    }

    /// The password.
    pub fn password(&self) -> &str {
        &self.password
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Parses a credentials file (module docs). The error says what is wrong,
/// never what the file contains. Pure.
pub fn parse_credentials(text: &str) -> Result<Credentials, &'static str> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let line = lines.next().ok_or("empty file")?;
    if lines.next().is_some() {
        return Err("more than one user:password line");
    }
    let (user, password) = line
        .split_once(':')
        .ok_or("no ':' between user and password")?;
    let (user, password) = (user.trim(), password.trim());
    if user.is_empty() {
        return Err("empty user");
    }
    if password.is_empty() {
        return Err("empty password");
    }
    Ok(Credentials::new(user, password))
}

/// Reads and parses the credentials file at `path`.
pub fn read_credentials(path: &Path) -> Result<Credentials, ServiceError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ServiceError::ZenohCredentials(format!("cannot read {}: {}", path.display(), e.kind()))
    })?;
    parse_credentials(&text).map_err(|e| ServiceError::ZenohCredentials(e.to_owned()))
}

/// What a run needs from Zenoh: nothing, or a router to connect to. Pure.
pub fn router_for<'c>(
    spec: &ServiceSpec,
    config: &'c Config,
) -> Result<Option<&'c str>, ServiceError> {
    match (spec.uses_zenoh(), config.zenoh_router.as_deref()) {
        (false, _) => Ok(None),
        (true, Some(router)) => Ok(Some(router)),
        (true, None) => Err(ServiceError::NoZenohRouter),
    }
}

/// JSON5 string literal of `s`, refusing characters that would need
/// escaping.
fn json_string(s: &str) -> Result<String, ZenohError> {
    if s.contains(['"', '\\']) || s.chars().any(char::is_control) {
        return Err(ZenohError::InvalidEndpoint);
    }
    Ok(format!("\"{s}\""))
}

/// The client-mode configuration for `router` with `credentials`.
pub fn client_config(
    router: &str,
    credentials: Option<&Credentials>,
) -> Result<::zenoh::Config, ZenohError> {
    let mut config = ::zenoh::Config::default();
    config.insert_json5("mode", r#""client""#)?;
    config.insert_json5("scouting/multicast/enabled", "false")?;
    config.insert_json5("connect/endpoints", &format!("[{}]", json_string(router)?))?;
    config.insert_json5("connect/timeout_ms", &CONNECT_TIMEOUT_MS.to_string())?;
    if let Some(c) = credentials {
        config.insert_json5("transport/auth/usrpwd/user", &json_string(&c.user)?)?;
        config.insert_json5(
            "transport/auth/usrpwd/password",
            &json_string(c.password())?,
        )?;
    }
    Ok(config)
}

/// Opens the run's session if the spec uses Zenoh (module docs).
pub async fn open(
    spec: &ServiceSpec,
    config: &Config,
) -> Result<Option<ZenohTransport>, ServiceError> {
    let Some(router) = router_for(spec, config)? else {
        return Ok(None);
    };
    let credentials = config
        .zenoh_credentials
        .as_deref()
        .map(read_credentials)
        .transpose()?;
    let zenoh_config = client_config(router, credentials.as_ref())?;
    let transport = ZenohTransport::open(zenoh_config).await?;
    tracing::info!(
        router,
        user = credentials.as_ref().map(|c| c.user.as_str()),
        "zenoh session open"
    );
    Ok(Some(transport))
}

/// Declares `hook`'s subscription on `transport` and calls the hook with
/// every sample until the session closes.
pub async fn run_hook(
    mut transport: ZenohTransport,
    mut hook: SampleHook,
) -> Result<(), ServiceError> {
    let filter = parse_filter(&hook.filter)
        .ok_or_else(|| ServiceError::Failed(format!("invalid filter {}", hook.filter)))?;
    let mut subscriber = transport.subscribe(filter).await?;
    let mut buf = vec![0u8; SAMPLE_BUFFER];
    // Each iteration delivers one sample or skips one that did not fit;
    // it ends when the session closes.
    loop {
        match subscriber.recv(&mut buf).await {
            Ok(sample) => hook.call(sample),
            Err(ZenohError::Closed) => return Ok(()),
            Err(e) => tracing::warn!(filter = hook.filter.as_str(), error = %e, "sample dropped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m10_credentials_file_is_one_user_password_line() {
        let c = parse_credentials("demo-caller:s3cr:et\n").unwrap();
        assert_eq!((c.user.as_str(), c.password()), ("demo-caller", "s3cr:et"));
        assert_eq!(
            parse_credentials("\n  u : p  \n\n").unwrap(),
            Credentials::new("u", "p")
        );
        assert_eq!(parse_credentials(""), Err("empty file"));
        assert_eq!(
            parse_credentials("nocolon"),
            Err("no ':' between user and password")
        );
        assert_eq!(parse_credentials(":p"), Err("empty user"));
        assert_eq!(parse_credentials("u:"), Err("empty password"));
        assert_eq!(
            parse_credentials("a:b\nc:d"),
            Err("more than one user:password line")
        );
        let shown = format!("{c:?}");
        assert!(
            shown.contains("demo-caller") && !shown.contains("s3cr"),
            "{shown}"
        );
    }

    #[test]
    fn m10_router_is_required_only_when_the_spec_uses_zenoh() {
        let quiet = ServiceSpec::new("demo/echo");
        let mut talks = ServiceSpec::new("demo/pub");
        talks.publishes.push("app/demo/**".into());
        let none = Config::default();
        let with = Config {
            zenoh_router: Some("tcp/zenohd.apps:7447".into()),
            ..Config::default()
        };
        assert!(matches!(router_for(&quiet, &none), Ok(None)));
        assert!(matches!(router_for(&quiet, &with), Ok(None)));
        assert!(matches!(
            router_for(&talks, &none),
            Err(ServiceError::NoZenohRouter)
        ));
        assert!(matches!(
            router_for(&talks, &with),
            Ok(Some("tcp/zenohd.apps:7447"))
        ));
    }

    #[test]
    fn m10_client_config_sets_mode_router_and_credentials() {
        let c = Credentials::new("svc", "pw");
        let config = client_config("tcp/127.0.0.1:7447", Some(&c)).unwrap();
        let text = format!("{config}");
        assert!(text.contains("\"client\""), "{text}");
        assert!(text.contains("tcp/127.0.0.1:7447"), "{text}");
        assert!(client_config("tcp/\"x", None).is_err());
        assert!(client_config("tcp/x", Some(&Credentials::new("a\\", "b"))).is_err());
    }
}
