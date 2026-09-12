use imap::types::Uid;
use native_tls::{TlsConnector, TlsStream};
use serde::{Deserialize, Serialize};
use serde_json::json;
use signal_hook::consts::{SIGINT, SIGTERM};
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fs;
use std::io;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

type AnyError = Box<dyn Error + Send + Sync>;
type Session = imap::Session<TlsStream<TcpStream>>;

#[derive(Debug, Deserialize)]
struct Config {
    gmail: GmailConfig,
    #[serde(default = "default_state_path")]
    state_path: PathBuf,
    webhooks: Vec<WebhookConfig>,
}

#[derive(Debug, Deserialize)]
struct GmailConfig {
    email: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct WebhookConfig {
    name: String,
    url: String,
    senders: HashSet<String>,
    bearer_token: Option<String>,
}

impl Config {
    fn load(path: &Path) -> Result<Self, AnyError> {
        let contents = fs::read_to_string(path)?;
        let mut config: Self = toml::from_str(&contents)?;
        if config.webhooks.is_empty() {
            return Err("configuration must contain at least one [[webhooks]] entry".into());
        }

        let mut names = HashSet::new();
        for webhook in &mut config.webhooks {
            webhook.name = webhook.name.trim().to_owned();
            webhook.senders = webhook
                .senders
                .iter()
                .map(|sender| normalize_email(sender))
                .filter(|sender| !sender.is_empty())
                .collect();
            if webhook.name.is_empty() || webhook.url.trim().is_empty() {
                return Err("each webhook requires a non-empty name and url".into());
            }
            if webhook.senders.is_empty() {
                return Err(
                    format!("webhook {} requires at least one sender", webhook.name).into(),
                );
            }
            if !names.insert(webhook.name.clone()) {
                return Err(format!("duplicate webhook name: {}", webhook.name).into());
            }
        }
        Ok(config)
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct State {
    uid_validity: u32,
    last_uid: Uid,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

struct Xoauth2<'a> {
    email: &'a str,
    token: &'a str,
}

impl imap::Authenticator for Xoauth2<'_> {
    type Response = String;

    fn process(&self, _: &[u8]) -> Self::Response {
        format!("user={}\x01auth=Bearer {}\x01\x01", self.email, self.token)
    }
}

fn main() -> Result<(), AnyError> {
    let config_path = env::var_os("CONFIG_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/config/config.toml"));
    let config = Config::load(&config_path)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&shutdown))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&shutdown))?;
    let http: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::NativeTls)
                .build(),
        )
        .timeout_global(Some(Duration::from_secs(30)))
        .input_buffer_size(8 * 1024)
        .output_buffer_size(8 * 1024)
        .max_idle_connections(2)
        .build()
        .into();

    log(
        "info",
        "starting",
        json!({"config": config_path, "webhooks": config.webhooks.len()}),
    );
    run(&config, &http, &shutdown);
    log("info", "stopped", json!({}));
    Ok(())
}

fn run(config: &Config, http: &ureq::Agent, shutdown: &AtomicBool) {
    let mut delay = 1;
    while !shutdown.load(Ordering::Relaxed) {
        match monitor(config, http, shutdown) {
            Ok(()) => break,
            Err(error) => {
                log(
                    "error",
                    "connection_lost",
                    json!({"error": error.to_string(), "retry_seconds": delay}),
                );
                interruptible_sleep(Duration::from_secs(delay), shutdown);
                delay = (delay * 2).min(60);
            }
        }
    }
}

fn monitor(config: &Config, http: &ureq::Agent, shutdown: &AtomicBool) -> Result<(), AnyError> {
    let token = refresh_access_token(&config.gmail, http)?;
    let tls = TlsConnector::builder().build()?;
    let client = imap::connect(("imap.gmail.com", 993), "imap.gmail.com", &tls)?;
    let mut session = client
        .authenticate(
            "XOAUTH2",
            &Xoauth2 {
                email: &config.gmail.email,
                token: &token,
            },
        )
        .map_err(|(error, _)| error)?;

    let mailbox = session.select("INBOX")?;
    let uid_validity = mailbox
        .uid_validity
        .ok_or("Gmail did not return UIDVALIDITY")?;
    let mut state = load_state(&config.state_path)?;

    if state.uid_validity != uid_validity {
        state = State {
            uid_validity,
            last_uid: mailbox.uid_next.unwrap_or(1).saturating_sub(1),
        };
        save_state(&config.state_path, &state)?;
        log(
            "info",
            "checkpoint_initialized",
            json!({"uid_validity": uid_validity, "last_uid": state.last_uid}),
        );
    }

    process_new_messages(&mut session, config, http, &mut state)?;
    log(
        "info",
        "imap_connected",
        json!({"account": config.gmail.email}),
    );

    while !shutdown.load(Ordering::Relaxed) {
        let outcome = {
            let idle = session.idle()?;
            idle.wait_with_timeout(Duration::from_secs(30))?
        };
        match outcome {
            imap::extensions::idle::WaitOutcome::MailboxChanged => {
                process_new_messages(&mut session, config, http, &mut state)?;
            }
            imap::extensions::idle::WaitOutcome::TimedOut => {}
        }
    }

    let _ = session.logout();
    Ok(())
}

fn process_new_messages(
    session: &mut Session,
    config: &Config,
    http: &ureq::Agent,
    state: &mut State,
) -> Result<(), AnyError> {
    let query = format!("UID {}:*", state.last_uid.saturating_add(1));
    let mut uids = session.uid_search(query)?.into_iter().collect::<Vec<_>>();
    uids.retain(|uid| *uid > state.last_uid);
    uids.sort_unstable();

    for uid in uids {
        let messages = session.uid_fetch(uid.to_string(), "UID ENVELOPE")?;
        let senders = messages
            .iter()
            .next()
            .and_then(|message| message.envelope())
            .and_then(|envelope| envelope.from.as_ref())
            .map(|addresses| {
                addresses
                    .iter()
                    .filter_map(|address| {
                        let (Some(mailbox), Some(host)) = (&address.mailbox, &address.host) else {
                            return None;
                        };
                        Some(normalize_email(&format!(
                            "{}@{}",
                            String::from_utf8_lossy(mailbox),
                            String::from_utf8_lossy(host)
                        )))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let matches = matching_webhooks(&config.webhooks, &senders);
        if !matches.is_empty() {
            let body_messages = session.uid_fetch(uid.to_string(), "UID BODY.PEEK[]")?;
            let raw_message = body_messages
                .iter()
                .next()
                .and_then(|message| message.body())
                .ok_or("Gmail did not return the requested message")?;
            let body = extract_plain_text(raw_message)?;

            for (webhook, sender) in matches {
                post_webhook(
                    config,
                    webhook,
                    http,
                    uid,
                    sender,
                    &body,
                    state.uid_validity,
                )?;
                log(
                    "info",
                    "webhook_sent",
                    json!({"webhook": webhook.name, "uid": uid, "sender": sender}),
                );
            }
        }

        state.last_uid = uid;
        save_state(&config.state_path, state)?;
    }
    Ok(())
}

fn extract_plain_text(raw_message: &[u8]) -> Result<String, mailparse::MailParseError> {
    fn collect(
        part: &mailparse::ParsedMail<'_>,
        bodies: &mut Vec<String>,
    ) -> Result<(), mailparse::MailParseError> {
        if part.get_content_disposition().disposition == mailparse::DispositionType::Attachment {
            return Ok(());
        }
        if part.ctype.mimetype.eq_ignore_ascii_case("text/plain") {
            bodies.push(part.get_body()?);
            return Ok(());
        }
        for subpart in &part.subparts {
            collect(subpart, bodies)?;
        }
        Ok(())
    }

    let parsed = mailparse::parse_mail(raw_message)?;
    let mut bodies = Vec::new();
    collect(&parsed, &mut bodies)?;
    Ok(bodies.join("\n"))
}

fn matching_webhooks<'a>(
    webhooks: &'a [WebhookConfig],
    senders: &'a [String],
) -> Vec<(&'a WebhookConfig, &'a str)> {
    webhooks
        .iter()
        .filter_map(|webhook| {
            senders
                .iter()
                .find(|sender| webhook.senders.contains(*sender))
                .map(|sender| (webhook, sender.as_str()))
        })
        .collect()
}

fn refresh_access_token(gmail: &GmailConfig, http: &ureq::Agent) -> Result<String, AnyError> {
    let mut response = http
        .post("https://oauth2.googleapis.com/token")
        .send_form([
            ("client_id", gmail.client_id.as_str()),
            ("client_secret", gmail.client_secret.as_str()),
            ("refresh_token", gmail.refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])?;
    let body = response.body_mut().read_to_string()?;
    let token: TokenResponse = serde_json::from_str(&body)?;
    Ok(token.access_token)
}

fn post_webhook(
    config: &Config,
    webhook: &WebhookConfig,
    http: &ureq::Agent,
    uid: Uid,
    sender: &str,
    body: &str,
    uid_validity: u32,
) -> Result<(), AnyError> {
    let idempotency_key = format!("gmail:{}:{uid_validity}:{uid}", webhook.name);
    let payload = serde_json::to_vec(&json!({
        "account": config.gmail.email,
        "sender": sender,
        "body": body,
        "uid": uid,
        "uid_validity": uid_validity,
    }))?;
    let mut request = http
        .post(&webhook.url)
        .header("content-type", "application/json")
        .header("idempotency-key", &idempotency_key);
    if let Some(token) = &webhook.bearer_token {
        request = request.header("authorization", &format!("Bearer {token}"));
    }
    request.send(payload)?;
    Ok(())
}

fn load_state(path: &Path) -> Result<State, AnyError> {
    match fs::read(path) {
        Ok(contents) => Ok(serde_json::from_slice(&contents)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_state(path: &Path, state: &State) -> Result<(), AnyError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec(state)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

fn default_state_path() -> PathBuf {
    PathBuf::from("/data/state.json")
}

fn normalize_email(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn interruptible_sleep(duration: Duration, shutdown: &AtomicBool) {
    for _ in 0..duration.as_secs() {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn log(level: &str, event: &str, fields: serde_json::Value) {
    eprintln!(
        "{}",
        json!({
            "level": level,
            "event": event,
            "fields": fields,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_webhooks() {
        let config: Config = toml::from_str(
            r#"
                [gmail]
                email = "me@gmail.com"
                client_id = "client"
                client_secret = "secret"
                refresh_token = "refresh"

                [[webhooks]]
                name = "alerts"
                url = "https://example.com/alerts"
                senders = ["alert@example.com"]

                [[webhooks]]
                name = "billing"
                url = "https://example.com/billing"
                senders = ["billing@example.com"]
            "#,
        )
        .unwrap();

        assert_eq!(config.webhooks.len(), 2);
        assert_eq!(config.state_path, PathBuf::from("/data/state.json"));
    }

    #[test]
    fn matches_each_route_independently() {
        let webhooks = vec![
            WebhookConfig {
                name: "one".into(),
                url: "https://example.com/one".into(),
                senders: HashSet::from(["a@example.com".into()]),
                bearer_token: None,
            },
            WebhookConfig {
                name: "two".into(),
                url: "https://example.com/two".into(),
                senders: HashSet::from(["b@example.com".into()]),
                bearer_token: None,
            },
        ];
        let senders = vec!["b@example.com".into()];
        let matches = matching_webhooks(&webhooks, &senders);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].0.name, "two");
    }

    #[test]
    fn extracts_plain_text_without_html_or_images() {
        let message = b"MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=outer\r\n\
\r\n\
--outer\r\n\
Content-Type: multipart/alternative; boundary=inner\r\n\
\r\n\
--inner\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Hello, plain text!=0A\r\n\
--inner\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<strong>HTML</strong>\r\n\
--inner--\r\n\
--outer\r\n\
Content-Type: image/png\r\n\
Content-Transfer-Encoding: base64\r\n\
Content-Disposition: attachment; filename=image.png\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--outer--\r\n";

        let body = extract_plain_text(message).unwrap();
        assert_eq!(body, "Hello, plain text!\n");
        assert!(!body.contains("HTML"));
        assert!(!body.contains("iVBOR"));
    }

    #[test]
    fn state_round_trips() {
        let state = State {
            uid_validity: 42,
            last_uid: 123,
        };
        let encoded = serde_json::to_vec(&state).unwrap();
        let decoded: State = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.uid_validity, 42);
        assert_eq!(decoded.last_uid, 123);
    }
}
