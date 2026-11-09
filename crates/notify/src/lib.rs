//! Delivery by email and Slack, and live events for the apps.

pub mod events;

pub use events::{AppEvent, EventBus};

use async_trait::async_trait;
use lettre::message::{Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use lighthouse_config::DeliveryConfig;
use lighthouse_core::agent::Delivery;
use lighthouse_vault::{Vault, VaultError};
use secrecy::ExposeSecret;
use serde_json::json;
use std::sync::Arc;

/// Errors during delivery. They never contain webhook URLs or passwords.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// The target is not configured.
    #[error("delivery target not configured: {0}")]
    NotConfigured(String),
    /// Sending failed.
    #[error("delivery to {target} failed: {detail}")]
    Send {
        /// Target name.
        target: String,
        /// What went wrong.
        detail: String,
    },
    /// The vault failed.
    #[error(transparent)]
    Vault(#[from] VaultError),
}

/// Delivers finished reports.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Delivers a report written in Markdown.
    async fn deliver(
        &self,
        target: &Delivery,
        subject: &str,
        markdown: &str,
    ) -> Result<(), NotifyError>;
}

/// An email to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mail {
    /// Recipient address.
    pub to: String,
    /// Subject line.
    pub subject: String,
    /// Plain text body.
    pub text: String,
    /// HTML body, sent as an alternative to the text if present.
    pub html: Option<String>,
}

/// Sends email. The server sends every mail through one mailer: verification,
/// invitations and reports delivered to `mail:` targets. [`SmtpMailer`] is the
/// default; programs built on these crates can supply their own.
#[async_trait]
pub trait Mailer: Send + Sync {
    /// Sends one mail. Errors never contain passwords.
    async fn send(&self, mail: &Mail) -> Result<(), NotifyError>;
}

/// Sends a built email.
#[async_trait]
pub trait MailSender: Send + Sync {
    /// Sends the message.
    async fn send(&self, message: Message) -> Result<(), String>;
}

struct SmtpSender {
    settings: lighthouse_config::SmtpConfig,
    vault: Vault,
}

#[async_trait]
impl MailSender for SmtpSender {
    async fn send(&self, message: Message) -> Result<(), String> {
        let password = self
            .vault
            .get("smtp")
            .await
            .map_err(|error| error.to_string())?;
        let password = password.ok_or("vault entry `smtp` is missing")?;
        let credentials = Credentials::new(
            self.settings.username.clone(),
            password.expose_secret().to_owned(),
        );
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&self.settings.host)
            .map_err(|error| error.to_string())?
            .port(self.settings.port)
            .credentials(credentials)
            .build();
        transport
            .send(message)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// Sends mail over SMTP from a fixed sender address, the default [`Mailer`].
pub struct SmtpMailer {
    from: Mailbox,
    sender: Arc<dyn MailSender>,
}

impl std::fmt::Debug for SmtpMailer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpMailer")
            .field("from", &self.from.to_string())
            .finish_non_exhaustive()
    }
}

impl SmtpMailer {
    /// Sends through the configured SMTP server, with the password stored in
    /// the vault as `smtp`.
    pub fn from_config(
        settings: lighthouse_config::SmtpConfig,
        vault: Vault,
    ) -> Result<Self, NotifyError> {
        let from = settings
            .from
            .parse()
            .map_err(|_| NotifyError::NotConfigured("smtp sender".to_owned()))?;
        Ok(Self::new(from, Arc::new(SmtpSender { settings, vault })))
    }

    /// Sends through another transport, for example in tests.
    pub fn new(from: Mailbox, sender: Arc<dyn MailSender>) -> Self {
        Self { from, sender }
    }
}

#[async_trait]
impl Mailer for SmtpMailer {
    async fn send(&self, mail: &Mail) -> Result<(), NotifyError> {
        let failed = |detail: String| NotifyError::Send {
            target: format!("mail:{}", mail.to),
            detail,
        };
        let to: Mailbox = mail
            .to
            .parse()
            .map_err(|_| failed("invalid address".to_owned()))?;
        let builder = Message::builder()
            .from(self.from.clone())
            .to(to)
            .subject(mail.subject.as_str());
        let message = match &mail.html {
            Some(html) => builder.multipart(MultiPart::alternative_plain_html(
                mail.text.clone(),
                html.clone(),
            )),
            None => builder
                .header(lettre::message::header::ContentType::TEXT_PLAIN)
                .body(mail.text.clone()),
        }
        .map_err(|error| failed(error.to_string()))?;
        self.sender.send(message).await.map_err(failed)
    }
}

/// Tells a notifier which Slack aliases exist now and where their webhooks are.
/// Implementations may answer per user.
pub trait SlackSource: Send + Sync {
    /// Aliases that may be used for the current work.
    fn aliases(&self) -> Vec<String>;
    /// Vaults holding `slack:<alias>`, the first that has it wins.
    fn vaults(&self) -> Vec<Vault>;
    /// Whether webhooks must be public `https` URLs, since tenants and people
    /// store them.
    fn guarded(&self) -> bool;
}

/// Delivers through the configured channels.
pub struct ChannelNotifier {
    mail: Option<Arc<dyn Mailer>>,
    slack_aliases: Vec<String>,
    slack_source: Option<Arc<dyn SlackSource>>,
    vault: Vault,
    http: reqwest::Client,
}

impl std::fmt::Debug for ChannelNotifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelNotifier")
            .field("slack_aliases", &self.slack_aliases)
            .finish_non_exhaustive()
    }
}

fn to_html(markdown: &str) -> String {
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new(markdown));
    html
}

impl ChannelNotifier {
    /// Creates a notifier from the configuration.
    pub fn from_config(config: DeliveryConfig, vault: Vault) -> Result<Self, NotifyError> {
        let mail_vault = vault.clone();
        Self::with_vaults(config, vault, mail_vault)
    }

    /// Creates a notifier for a tenant: Slack webhooks come from the tenant's
    /// vault, the SMTP password from the instance's, since the mail server is
    /// configured once for the instance.
    pub fn with_vaults(
        config: DeliveryConfig,
        vault: Vault,
        mail_vault: Vault,
    ) -> Result<Self, NotifyError> {
        let slack_aliases = config.slack_channels;
        let mail = config
            .smtp
            .map(|settings| {
                SmtpMailer::from_config(settings, mail_vault)
                    .map(|mailer| Arc::new(mailer) as Arc<dyn Mailer>)
            })
            .transpose()?;
        Ok(Self {
            mail,
            slack_aliases,
            vault,
            http: reqwest::Client::new(),
            slack_source: None,
        })
    }

    /// Reads Slack aliases and webhooks from a source instead of the configuration.
    #[must_use]
    pub fn with_slack(mut self, source: Arc<dyn SlackSource>) -> Self {
        self.slack_source = Some(source);
        self
    }

    /// Replaces the mail transport.
    pub fn with_mail_sender(self, from: Mailbox, sender: Arc<dyn MailSender>) -> Self {
        self.with_mailer(Arc::new(SmtpMailer::new(from, sender)))
    }

    /// Sends mail through another mailer instead of the configured SMTP server.
    pub fn with_mailer(mut self, mailer: Arc<dyn Mailer>) -> Self {
        self.mail = Some(mailer);
        self
    }

    async fn mail(&self, address: &str, subject: &str, markdown: &str) -> Result<(), NotifyError> {
        let mailer = self
            .mail
            .as_ref()
            .ok_or_else(|| NotifyError::NotConfigured("smtp".to_owned()))?;
        mailer
            .send(&Mail {
                to: address.to_owned(),
                subject: subject.to_owned(),
                text: markdown.to_owned(),
                html: Some(to_html(markdown)),
            })
            .await
    }

    async fn slack(&self, alias: &str, subject: &str, markdown: &str) -> Result<(), NotifyError> {
        let target = format!("slack:{alias}");
        let (aliases, vaults, guarded) = match &self.slack_source {
            Some(source) => (source.aliases(), source.vaults(), source.guarded()),
            None => (self.slack_aliases.clone(), vec![self.vault.clone()], false),
        };
        if !aliases.iter().any(|known| known == alias) {
            return Err(NotifyError::NotConfigured(target));
        }
        let mut found = None;
        for vault in &vaults {
            found = vault.get(&target).await?;
            if found.is_some() {
                break;
            }
        }
        let url = found.ok_or_else(|| NotifyError::NotConfigured(target.clone()))?;
        let failed = |detail: String| NotifyError::Send {
            target: target.clone(),
            detail,
        };
        let http = if guarded {
            lighthouse_router::net::check_url(url.expose_secret())
                .map_err(|_| failed("the webhook is not a public https URL".to_owned()))?;
            lighthouse_router::net::guarded_client()
        } else {
            self.http.clone()
        };
        let response = http
            .post(url.expose_secret())
            .json(&json!({"text": format!("*{subject}*\n\n{markdown}")}))
            .send()
            .await
            .map_err(|error| failed(error.without_url().to_string()))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(failed(format!("status {}", response.status().as_u16())))
        }
    }
}

#[async_trait]
impl Notifier for ChannelNotifier {
    async fn deliver(
        &self,
        target: &Delivery,
        subject: &str,
        markdown: &str,
    ) -> Result<(), NotifyError> {
        match target {
            Delivery::Mail(address) => self.mail(address, subject, markdown).await,
            Delivery::Slack(alias) => self.slack(alias, subject, markdown).await,
            Delivery::Owner => Err(NotifyError::NotConfigured("owner".to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Slack(Vec<Vault>, bool);

    impl SlackSource for Slack {
        fn aliases(&self) -> Vec<String> {
            vec!["#team".to_owned()]
        }
        fn vaults(&self) -> Vec<Vault> {
            self.0.clone()
        }
        fn guarded(&self) -> bool {
            self.1
        }
    }

    use base64::Engine;
    use lighthouse_core::TenantId;
    use lighthouse_store::testing::fresh_store;
    use lighthouse_vault::MasterKey;
    use secrecy::SecretString;
    use std::sync::Mutex;
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Default)]
    struct RecordingSender {
        sent: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl MailSender for RecordingSender {
        async fn send(&self, message: Message) -> Result<(), String> {
            self.sent
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&message.formatted()).into_owned());
            Ok(())
        }
    }

    async fn vault() -> Vault {
        let key = base64::engine::general_purpose::STANDARD.encode([3u8; 32]);
        Vault::new(
            fresh_store().await,
            TenantId::DEFAULT,
            &MasterKey::from_base64(&key).unwrap(),
        )
    }

    fn config(aliases: &[&str]) -> DeliveryConfig {
        DeliveryConfig {
            smtp: None,
            slack_channels: aliases.iter().map(|alias| (*alias).to_owned()).collect(),
        }
    }

    #[tokio::test]
    async fn webhooks_of_a_source_come_from_its_vaults_and_stay_public() {
        let store = fresh_store().await;
        let key = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        let vault = Vault::new(
            store,
            TenantId::DEFAULT,
            &MasterKey::from_base64(&key).unwrap(),
        );
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        vault
            .put("slack:#team", &SecretString::from(server.uri()))
            .await
            .unwrap();
        let config = DeliveryConfig::default();
        let open = ChannelNotifier::from_config(config, vault.clone())
            .unwrap()
            .with_slack(Arc::new(Slack(vec![vault.clone()], false)));
        let target = Delivery::Slack("#team".to_owned());
        open.deliver(&target, "s", "b").await.unwrap();
        assert!(matches!(
            open.deliver(&Delivery::Slack("#other".to_owned()), "s", "b")
                .await,
            Err(NotifyError::NotConfigured(_))
        ));
        let guarded = ChannelNotifier::from_config(DeliveryConfig::default(), vault.clone())
            .unwrap()
            .with_slack(Arc::new(Slack(vec![vault], true)));
        let error = guarded.deliver(&target, "s", "b").await.unwrap_err();
        assert!(
            error.to_string().contains("not a public https URL"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn mail_contains_plain_text_and_html() {
        let sender = Arc::new(RecordingSender::default());
        let notifier = ChannelNotifier::from_config(config(&[]), vault().await)
            .unwrap()
            .with_mail_sender("from@example.com".parse().unwrap(), sender.clone());
        let target = Delivery::Mail("to@example.com".to_owned());
        notifier
            .deliver(&target, "Brief", "# Today\n\n**Bold**")
            .await
            .unwrap();
        let sent = sender.sent.lock().unwrap();
        assert!(sent[0].contains("Subject: Brief"));
        assert!(sent[0].contains("text/plain") && sent[0].contains("text/html"));
        assert!(sent[0].contains("<strong>Bold</strong>"));
    }

    #[tokio::test]
    async fn mail_needs_configuration_and_a_valid_address() {
        let notifier = ChannelNotifier::from_config(config(&[]), vault().await).unwrap();
        let error = notifier
            .deliver(&Delivery::Mail("a@example.com".to_owned()), "s", "b")
            .await
            .unwrap_err();
        assert!(matches!(error, NotifyError::NotConfigured(_)));
        let notifier = notifier.with_mail_sender(
            "from@example.com".parse().unwrap(),
            Arc::new(RecordingSender::default()),
        );
        let error = notifier
            .deliver(&Delivery::Mail("not an address".to_owned()), "s", "b")
            .await
            .unwrap_err();
        assert!(matches!(error, NotifyError::Send { .. }));
    }

    #[derive(Default)]
    struct RecordingMailer {
        sent: Mutex<Vec<Mail>>,
    }

    #[async_trait]
    impl Mailer for RecordingMailer {
        async fn send(&self, mail: &Mail) -> Result<(), NotifyError> {
            self.sent.lock().unwrap().push(mail.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn another_mailer_replaces_smtp() {
        let mailer = Arc::new(RecordingMailer::default());
        let notifier = ChannelNotifier::from_config(config(&[]), vault().await)
            .unwrap()
            .with_mailer(mailer.clone());
        notifier
            .deliver(
                &Delivery::Mail("to@example.com".to_owned()),
                "Brief",
                "*hi*",
            )
            .await
            .unwrap();
        let sent = mailer.sent.lock().unwrap();
        assert_eq!(sent[0].to, "to@example.com");
        assert_eq!(sent[0].text, "*hi*");
        assert!(sent[0].html.as_deref().unwrap().contains("<em>hi</em>"));
    }

    #[tokio::test]
    async fn smtp_sends_plain_text_alone_without_html() {
        let sender = Arc::new(RecordingSender::default());
        let mailer = SmtpMailer::new("from@example.com".parse().unwrap(), sender.clone());
        assert!(format!("{mailer:?}").contains("from@example.com"));
        let mail = Mail {
            to: "to@example.com".to_owned(),
            subject: "Hello".to_owned(),
            text: "Just text".to_owned(),
            html: None,
        };
        mailer.send(&mail).await.unwrap();
        let sent = sender.sent.lock().unwrap()[0].clone();
        assert!(sent.contains("text/plain") && !sent.contains("text/html"));
        assert!(sent.contains("Just text"));
        let wrong = Mail {
            to: "nobody".to_owned(),
            ..mail
        };
        assert!(matches!(
            mailer.send(&wrong).await,
            Err(NotifyError::Send { .. })
        ));
    }

    #[tokio::test]
    async fn smtp_needs_a_valid_sender() {
        let settings = lighthouse_config::SmtpConfig {
            host: "localhost".to_owned(),
            port: 587,
            username: "user".to_owned(),
            from: "not an address".to_owned(),
        };
        assert!(matches!(
            SmtpMailer::from_config(settings, vault().await),
            Err(NotifyError::NotConfigured(_))
        ));
    }

    #[tokio::test]
    async fn slack_posts_to_the_webhook_from_the_vault() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"text": "*Weekly*\n\nAll good"})))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let vault = vault().await;
        vault
            .put("slack:#weekly", &SecretString::from(server.uri()))
            .await
            .unwrap();
        let notifier = ChannelNotifier::from_config(config(&["#weekly"]), vault).unwrap();
        notifier
            .deliver(&Delivery::Slack("#weekly".to_owned()), "Weekly", "All good")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn slack_errors_do_not_leak_the_webhook() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let vault = vault().await;
        vault
            .put(
                "slack:#a",
                &SecretString::from(format!("{}/secret-path", server.uri())),
            )
            .await
            .unwrap();
        vault
            .put(
                "slack:#b",
                &SecretString::from("http://127.0.0.1:1/secret-path"),
            )
            .await
            .unwrap();
        let notifier = ChannelNotifier::from_config(config(&["#a", "#b"]), vault).unwrap();
        for alias in ["#a", "#b"] {
            let error = notifier
                .deliver(&Delivery::Slack(alias.to_owned()), "s", "b")
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("secret-path"), "{error}");
        }
    }

    #[tokio::test]
    async fn slack_rejects_unknown_aliases_and_missing_webhooks() {
        let notifier = ChannelNotifier::from_config(config(&["#known"]), vault().await).unwrap();
        for alias in ["#other", "#known"] {
            let error = notifier
                .deliver(&Delivery::Slack(alias.to_owned()), "s", "b")
                .await
                .unwrap_err();
            assert!(matches!(error, NotifyError::NotConfigured(_)));
        }
    }
}
