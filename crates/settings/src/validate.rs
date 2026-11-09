//! Checks of what tenants and people enter.

use crate::SettingsError;
use lighthouse_config::ProviderKind;

/// Tasks the server routes to models.
pub const KNOWN_TASKS: &[&str] = &["text"];

/// Names a provider or connector may not take: vault entries of the instance
/// and prefixes of built-in tools.
const RESERVED: &[&str] = &["smtp", "brain", "tasks", "memory", "posts", "provider"];

const MAX_NAME: usize = 40;
const MAX_MODEL: usize = 200;
const MAX_TOOLS: usize = 200;
const MAX_TOOL: usize = 100;
const MAX_SCOPES: usize = 30;
const MAX_TEXT: usize = 200;
const MAX_URL: usize = 2000;
/// The highest price per million tokens that is still plausible, in EUR.
const MAX_PRICE: f64 = 10_000.0;

fn invalid(message: impl Into<String>) -> SettingsError {
    SettingsError::Invalid(message.into())
}

/// Whether a text is a valid name: a lower-case letter, then up to 39 lower-case
/// letters, digits, `-` or `_`.
pub fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|first| first.is_ascii_lowercase())
        && text.len() <= MAX_NAME
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Checks the name of a provider or connector.
pub(crate) fn name(what: &str, text: &str) -> Result<(), SettingsError> {
    if !is_name(text) {
        return Err(invalid(format!(
            "the {what} name must start with a lower-case letter and have at most {MAX_NAME} \
             lower-case letters, digits, `-` or `_`"
        )));
    }
    if RESERVED.contains(&text) {
        return Err(invalid(format!("`{text}` is reserved")));
    }
    Ok(())
}

/// Checks a task name.
pub(crate) fn task(text: &str) -> Result<(), SettingsError> {
    if KNOWN_TASKS.contains(&text) {
        Ok(())
    } else {
        Err(invalid(format!(
            "unknown task `{}`, known are: {}",
            text.chars().take(MAX_NAME).collect::<String>(),
            KNOWN_TASKS.join(", ")
        )))
    }
}

/// Reads a provider kind. Mock and demo providers exist only in `lighthouse.toml`.
pub(crate) fn kind(text: &str) -> Result<ProviderKind, SettingsError> {
    match text {
        "anthropic" => Ok(ProviderKind::Anthropic),
        "openai" => Ok(ProviderKind::Openai),
        _ => Err(invalid("the kind must be `anthropic` or `openai`")),
    }
}

/// Checks a model name.
pub(crate) fn model(text: &str) -> Result<(), SettingsError> {
    let text = text.trim();
    if text.is_empty() || text.len() > MAX_MODEL || text.chars().any(char::is_control) {
        return Err(invalid(format!(
            "the model name must have 1 to {MAX_MODEL} printable characters"
        )));
    }
    Ok(())
}

/// Checks a price in EUR per million tokens.
pub(crate) fn price(what: &str, value: f64) -> Result<(), SettingsError> {
    if value.is_finite() && (0.0..=MAX_PRICE).contains(&value) {
        Ok(())
    } else {
        Err(invalid(format!(
            "`{what}` must be a price from 0 to {MAX_PRICE} EUR per million tokens"
        )))
    }
}

/// Checks a list of tool names.
pub(crate) fn tools(what: &str, tools: &[String]) -> Result<(), SettingsError> {
    if tools.len() > MAX_TOOLS {
        return Err(invalid(format!("`{what}` has more than {MAX_TOOLS} tools")));
    }
    for tool in tools {
        let valid = !tool.is_empty()
            && tool.len() <= MAX_TOOL
            && tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'));
        if !valid {
            return Err(invalid(format!(
                "`{what}` holds an invalid tool name; use letters, digits, `_`, `-`, `.` or `/`"
            )));
        }
    }
    Ok(())
}

/// Checks OAuth scopes.
pub(crate) fn scopes(scopes: &[String]) -> Result<(), SettingsError> {
    if scopes.len() > MAX_SCOPES {
        return Err(invalid(format!("at most {MAX_SCOPES} scopes")));
    }
    if scopes.iter().any(|scope| {
        scope.is_empty()
            || scope.len() > MAX_TEXT
            || scope.chars().any(|c| c.is_whitespace() || c.is_control())
    }) {
        return Err(invalid("scopes must be single words without spaces"));
    }
    Ok(())
}

/// Checks an OAuth client ID.
pub(crate) fn client_id(text: Option<&str>) -> Result<(), SettingsError> {
    match text {
        Some(text)
            if text.is_empty()
                || text.len() > MAX_TEXT
                || text.chars().any(|c| c.is_whitespace() || c.is_control()) =>
        {
            Err(invalid("the client ID must be a single word"))
        }
        _ => Ok(()),
    }
}

/// Checks an endpoint a tenant or person chose and returns it normalized.
///
/// Without `allow_private`, it must be a public `https` URL whose host
/// resolves to public addresses only. With it, any `http` or `https` URL
/// without credentials passes.
pub(crate) async fn endpoint(text: &str, allow_private: bool) -> Result<String, SettingsError> {
    if text.len() > MAX_URL {
        return Err(invalid("the URL is too long"));
    }
    if allow_private {
        let url = url_parse(text)?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            return Err(invalid("the URL must use http or https"));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(invalid("the URL must not contain credentials"));
        }
        return Ok(url.to_string());
    }
    lighthouse_router::net::check_endpoint(text)
        .await
        .map(|url| url.to_string())
        .map_err(|error| invalid(error.to_string()))
}

fn url_parse(text: &str) -> Result<lighthouse_router::net::Url, SettingsError> {
    lighthouse_router::net::Url::parse(text.trim()).map_err(|_| invalid("the URL is not valid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_identifiers_and_not_reserved() {
        assert!(is_name("anthropic") && is_name("my-llm_2"));
        for bad in ["", "Anthropic", "1x", "a b", "a:b", &"x".repeat(41)] {
            assert!(!is_name(bad), "{bad}");
        }
        assert!(name("provider", "smtp").is_err());
        assert!(name("connector", "brain").is_err());
        assert!(name("connector", "linear").is_ok());
    }

    #[test]
    fn values_are_checked() {
        assert!(task("text").is_ok() && task("image").is_err());
        assert!(kind("openai").is_ok() && kind("mock").is_err());
        assert!(model("some-model").is_ok() && model(" ").is_err() && model("a\nb").is_err());
        assert!(price("input", 0.0).is_ok() && price("input", 3.5).is_ok());
        for bad in [-1.0, f64::NAN, f64::INFINITY, 20_000.0] {
            assert!(price("input", bad).is_err());
        }
        assert!(tools("read_tools", &["list_issues".to_owned()]).is_ok());
        assert!(tools("read_tools", &["drop table".to_owned()]).is_err());
        assert!(tools("read_tools", &vec!["a".to_owned(); 201]).is_err());
        assert!(scopes(&["read".to_owned()]).is_ok() && scopes(&["a b".to_owned()]).is_err());
        assert!(client_id(Some("abc")).is_ok() && client_id(Some("")).is_err());
        assert!(client_id(None).is_ok());
    }

    #[tokio::test]
    async fn endpoints_must_be_public_unless_allowed() {
        assert!(endpoint("http://10.0.0.5:8080/mcp", false).await.is_err());
        assert!(endpoint("https://127.0.0.1/mcp", false).await.is_err());
        assert_eq!(
            endpoint("http://10.0.0.5:8080/mcp", true).await.unwrap(),
            "http://10.0.0.5:8080/mcp"
        );
        assert!(endpoint("ftp://example.com", true).await.is_err());
        assert!(endpoint("https://a:b@example.com", true).await.is_err());
        assert!(endpoint("nonsense", true).await.is_err());
        assert!(endpoint(&"x".repeat(3000), true).await.is_err());
    }
}
