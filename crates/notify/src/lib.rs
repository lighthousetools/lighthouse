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

/// Delivers through the configured channels.
pub struct ChannelNotifier {
    mail: Option<(Mailbox, Arc<dyn MailSender>)>,
    slack_aliases: Vec<String>,
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
        let slack_aliases = config.slack_channels;
        let mail = config
            .smtp
            .map(|settings| {
                let from = settings
                    .from
                    .parse()
                    .map_err(|_| NotifyError::NotConfigured("smtp sender".to_owned()))?;
                let sender: Arc<dyn MailSender> = Arc::new(SmtpSender {
                    settings,
                    vault: vault.clone(),
                });
                Ok::<_, NotifyError>((from, sender))
            })
            .transpose()?;
        Ok(Self {
            mail,
            slack_aliases,
            vault,
            http: reqwest::Client::new(),
        })
    }

    /// Replaces the mail transport.
    pub fn with_mail_sender(mut self, from: Mailbox, sender: Arc<dyn MailSender>) -> Self {
        self.mail = Some((from, sender));
        self
    }

    async fn mail(&self, address: &str, subject: &str, markdown: &str) -> Result<(), NotifyError> {
        let failed = |detail: String| NotifyError::Send {
            target: format!("mail:{address}"),
            detail,
        };
        let (from, sender) = self
            .mail
            .as_ref()
            .ok_or_else(|| NotifyError::NotConfigured("smtp".to_owned()))?;
        let to: Mailbox = address
            .parse()
            .map_err(|_| failed("invalid address".to_owned()))?;
        let message = Message::builder()
            .from(from.clone())
            .to(to)
            .subject(subject)
            .multipart(MultiPart::alternative_plain_html(
                markdown.to_owned(),
                to_html(markdown),
            ))
            .map_err(|error| failed(error.to_string()))?;
        sender.send(message).await.map_err(failed)
    }

    async fn slack(&self, alias: &str, subject: &str, markdown: &str) -> Result<(), NotifyError> {
        let target = format!("slack:{alias}");
        if !self.slack_aliases.iter().any(|known| known == alias) {
            return Err(NotifyError::NotConfigured(target));
        }
        let url = self
            .vault
            .get(&target)
            .await?
            .ok_or_else(|| NotifyError::NotConfigured(target.clone()))?;
        let failed = |detail: String| NotifyError::Send {
            target: target.clone(),
            detail,
        };
        let response = self
            .http
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
