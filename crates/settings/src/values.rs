//! Single settings by key, such as notifications or the Talk agent.

use crate::SettingsError;
use lighthouse_config::{Config, NotificationsConfig, WeatherConfig};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The Talk agent: an agent ID, or `null` so the Second Brain answers.
pub const TALK_AGENT: &str = "talk.agent";
/// What is pushed through the delivery channels: `[notifications]`.
pub const NOTIFICATIONS: &str = "notifications";
/// Where someone is, for the weather in briefs: latitude, longitude and location.
pub const WEATHER: &str = "weather";
/// IANA time zone shown in the apps.
pub const TIMEZONE: &str = "timezone";
/// Language of the apps: `en` or `de`.
pub const LANGUAGE: &str = "language";
/// A person's names in services, by connector: `{"linear": "alex"}`. Personal
/// only. Sources use them to recognize what is the person's own.
pub const ACCOUNTS: &str = "accounts";
/// A person's further email addresses, to recognize their own mail. Personal only.
pub const ADDRESSES: &str = "addresses";
/// Every key that can be set.
pub const VALUE_KEYS: &[&str] = &[
    TALK_AGENT,
    NOTIFICATIONS,
    WEATHER,
    TIMEZONE,
    LANGUAGE,
    ACCOUNTS,
    ADDRESSES,
];

const MAX_TARGETS: usize = 10;
const MAX_TEXT: usize = 100;
const MAX_ACCOUNTS: usize = 50;

fn invalid(message: impl Into<String>) -> SettingsError {
    SettingsError::Invalid(message.into())
}

/// The defaults that `lighthouse.toml` gives for single settings.
pub(crate) fn instance_values(config: &Config) -> BTreeMap<String, Value> {
    let mut values = BTreeMap::from([
        (TIMEZONE.to_owned(), json!(config.server.timezone)),
        (TALK_AGENT.to_owned(), json!(config.talk.agent)),
        (
            NOTIFICATIONS.to_owned(),
            json!({
                "deliver": config.notifications.deliver,
                "decisions": config.notifications.decisions,
                "done": config.notifications.done,
                "alerts": config.notifications.alerts,
            }),
        ),
    ]);
    if let Some(weather) = &config.weather {
        values.insert(
            WEATHER.to_owned(),
            json!({"latitude": weather.latitude, "longitude": weather.longitude,
                   "location": weather.location}),
        );
    }
    values
}

fn check_target(target: &str, personal: bool) -> Result<(), SettingsError> {
    if target == "owner" {
        return Ok(());
    }
    if let Some(address) = target.strip_prefix("mail:") {
        // A person could otherwise make the server mail anyone.
        if personal {
            return Err(invalid(
                "your own notifications go to `owner` or a Slack alias, not to other addresses",
            ));
        }
        if address.contains('@')
            && address.len() <= MAX_TEXT
            && !address.contains(char::is_whitespace)
        {
            return Ok(());
        }
    }
    if let Some(alias) = target.strip_prefix("slack:")
        && !alias.is_empty()
        && alias.len() <= MAX_TEXT
        && !alias.chars().any(char::is_control)
    {
        return Ok(());
    }
    Err(invalid(
        "notification targets are `owner`, `mail:<address>` or `slack:<alias>`",
    ))
}

/// Checks a value for a key and returns it in its stored form.
pub fn check_value(key: &str, value: &Value, personal: bool) -> Result<Value, SettingsError> {
    match key {
        TALK_AGENT => match value {
            Value::Null => Ok(Value::Null),
            Value::String(agent)
                if !agent.is_empty()
                    && agent.len() <= MAX_TEXT
                    && agent
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) =>
            {
                Ok(value.clone())
            }
            _ => Err(invalid("`talk.agent` is an agent ID or null")),
        },
        NOTIFICATIONS => {
            let parsed: NotificationsConfig = serde_json::from_value(value.clone())
                .map_err(|error| invalid(format!("invalid notifications: {error}")))?;
            if parsed.deliver.len() > MAX_TARGETS {
                return Err(invalid(format!("at most {MAX_TARGETS} targets")));
            }
            for target in &parsed.deliver {
                check_target(target, personal)?;
            }
            if !["info", "warning", "critical"].contains(&parsed.alerts.as_str()) {
                return Err(invalid("`alerts` is `info`, `warning` or `critical`"));
            }
            Ok(
                json!({"deliver": parsed.deliver, "decisions": parsed.decisions,
                      "done": parsed.done, "alerts": parsed.alerts}),
            )
        }
        WEATHER => {
            let parsed: WeatherConfig = serde_json::from_value(value.clone())
                .map_err(|error| invalid(format!("invalid weather: {error}")))?;
            // The server stays the one that decides which weather service it calls.
            if parsed.base_url.is_some() {
                return Err(invalid("`base_url` can only be set in lighthouse.toml"));
            }
            let in_range = (-90.0..=90.0).contains(&parsed.latitude)
                && (-180.0..=180.0).contains(&parsed.longitude);
            if !in_range {
                return Err(invalid("latitude and longitude are out of range"));
            }
            if parsed.location.as_deref().is_some_and(|location| {
                location.len() > MAX_TEXT || location.chars().any(char::is_control)
            }) {
                return Err(invalid("the location is too long"));
            }
            Ok(
                json!({"latitude": parsed.latitude, "longitude": parsed.longitude,
                      "location": parsed.location}),
            )
        }
        TIMEZONE => match value.as_str() {
            Some(zone) if zone.parse::<chrono_tz::Tz>().is_ok() => Ok(value.clone()),
            _ => Err(invalid(
                "`timezone` is an IANA time zone such as Europe/Berlin",
            )),
        },
        LANGUAGE => match value.as_str() {
            Some("en" | "de") => Ok(value.clone()),
            _ => Err(invalid("`language` is `en` or `de`")),
        },
        ACCOUNTS | ADDRESSES if !personal => Err(invalid(format!(
            "`{key}` belongs to a person, not to the workspace"
        ))),
        ACCOUNTS => {
            let accounts = value
                .as_object()
                .ok_or_else(|| invalid("`accounts` maps connectors to account names"))?;
            if accounts.len() > MAX_ACCOUNTS {
                return Err(invalid(format!("at most {MAX_ACCOUNTS} accounts")));
            }
            for (service, account) in accounts {
                let valid = service.len() <= MAX_TEXT
                    && account.as_str().is_some_and(|name| {
                        !name.is_empty()
                            && name.len() <= MAX_TEXT
                            && !name.chars().any(char::is_control)
                    });
                if !valid {
                    return Err(invalid("`accounts` maps connectors to account names"));
                }
            }
            Ok(value.clone())
        }
        ADDRESSES => {
            let addresses = value
                .as_array()
                .ok_or_else(|| invalid("`addresses` is a list of email addresses"))?;
            if addresses.len() > MAX_ACCOUNTS {
                return Err(invalid(format!("at most {MAX_ACCOUNTS} addresses")));
            }
            let valid = addresses.iter().all(|address| {
                address.as_str().is_some_and(|text| {
                    text.contains('@')
                        && text.len() <= MAX_TEXT
                        && !text.contains(char::is_whitespace)
                })
            });
            if !valid {
                return Err(invalid("`addresses` is a list of email addresses"));
            }
            Ok(value.clone())
        }
        _ => Err(SettingsError::NotFound("setting".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_checked_by_key() {
        assert_eq!(
            check_value(TALK_AGENT, &json!("assistant"), false).unwrap(),
            json!("assistant")
        );
        assert_eq!(
            check_value(TALK_AGENT, &Value::Null, true).unwrap(),
            Value::Null
        );
        assert!(check_value(TALK_AGENT, &json!("no spaces"), false).is_err());
        assert!(check_value(TALK_AGENT, &json!(3), false).is_err());
        let push = json!({"deliver": ["owner", "slack:#team"], "done": true, "alerts": "warning"});
        let stored = check_value(NOTIFICATIONS, &push, true).unwrap();
        assert_eq!(stored["decisions"], true, "the default fills in");
        let mail = json!({"deliver": ["mail:team@example.com"]});
        assert!(check_value(NOTIFICATIONS, &mail, false).is_ok());
        assert!(
            check_value(NOTIFICATIONS, &mail, true).is_err(),
            "no relay for members"
        );
        for bad in [
            json!({"deliver": ["fax:1"]}),
            json!({"deliver": ["mail:nobody"]}),
            json!({"alerts": "loud"}),
            json!({"unknown": 1}),
            json!({"deliver": vec!["owner"; 11]}),
        ] {
            assert!(check_value(NOTIFICATIONS, &bad, false).is_err(), "{bad}");
        }
        let place = json!({"latitude": 52.5, "longitude": 13.4, "location": "Berlin"});
        assert!(check_value(WEATHER, &place, true).is_ok());
        for bad in [
            json!({"latitude": 91.0, "longitude": 0.0}),
            json!({"latitude": 0.0, "longitude": 181.0}),
            json!({"latitude": 0.0, "longitude": 0.0, "base_url": "http://10.0.0.1"}),
            json!({"latitude": 0.0, "longitude": 0.0, "location": "x".repeat(101)}),
            json!("Berlin"),
        ] {
            assert!(check_value(WEATHER, &bad, true).is_err(), "{bad}");
        }
        assert!(check_value(TIMEZONE, &json!("Europe/Berlin"), true).is_ok());
        assert!(check_value(TIMEZONE, &json!("Mars/Base"), true).is_err());
        assert!(check_value(LANGUAGE, &json!("de"), true).is_ok());
        assert!(check_value(LANGUAGE, &json!("fr"), true).is_err());
        assert!(check_value(ACCOUNTS, &json!({"linear": "alex"}), true).is_ok());
        assert!(check_value(ACCOUNTS, &json!({"linear": "alex"}), false).is_err());
        assert!(check_value(ACCOUNTS, &json!({"linear": 1}), true).is_err());
        assert!(check_value(ACCOUNTS, &json!(["x"]), true).is_err());
        assert!(check_value(ADDRESSES, &json!(["a@example.com"]), true).is_ok());
        assert!(check_value(ADDRESSES, &json!(["nope"]), true).is_err());
        assert!(check_value(ADDRESSES, &json!("a@example.com"), true).is_err());
        assert!(matches!(
            check_value("server.bind", &json!("x"), false),
            Err(SettingsError::NotFound(_))
        ));
    }
}
