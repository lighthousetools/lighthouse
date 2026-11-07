//! Today's weather from Open-Meteo (<https://open-meteo.com>), which needs no
//! key. Only the configured coordinates leave the server.

use lighthouse_config::WeatherConfig;
use lighthouse_desk::Weather;
use serde_json::Value;
use std::time::Duration;

const DEFAULT_BASE: &str = "https://api.open-meteo.com";
const TIMEOUT: Duration = Duration::from_secs(10);

/// Words for the WMO weather codes Open-Meteo uses.
pub fn describe(code: i64) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51..=57 => "Drizzle",
        61..=67 => "Rain",
        71..=77 => "Snow",
        80..=82 => "Rain showers",
        85 | 86 => "Snow showers",
        95..=99 => "Thunderstorm",
        _ => "Unknown",
    }
}

/// Reads an Open-Meteo answer.
pub fn parse(body: &Value, location: Option<String>) -> Option<Weather> {
    let current = &body["current"];
    let code = current["weather_code"].as_i64()?;
    let first = |key: &str| body["daily"][key][0].as_f64();
    Some(Weather {
        summary: describe(code).to_owned(),
        temperature_c: current["temperature_2m"].as_f64(),
        high_c: first("temperature_2m_max"),
        low_c: first("temperature_2m_min"),
        location,
        source: Some("Open-Meteo".to_owned()),
    })
}

/// Fetches today's weather. Errors are logged and give `None`.
pub async fn today(config: &WeatherConfig) -> Option<Weather> {
    let base = config
        .base_url
        .as_deref()
        .unwrap_or(DEFAULT_BASE)
        .trim_end_matches('/');
    let url = format!(
        "{base}/v1/forecast?latitude={}&longitude={}&current=temperature_2m,weather_code\
         &daily=temperature_2m_max,temperature_2m_min&timezone=auto&forecast_days=1",
        config.latitude, config.longitude
    );
    let client = reqwest::Client::builder().timeout(TIMEOUT).build().ok()?;
    let answer: Result<Value, reqwest::Error> = async {
        let response = client.get(&url).send().await?.error_for_status()?;
        response.json::<Value>().await
    }
    .await;
    match answer {
        Ok(body) => {
            let weather = parse(&body, config.location.clone());
            if weather.is_none() {
                tracing::warn!("the weather service answered without a weather code");
            }
            weather
        }
        Err(error) => {
            tracing::warn!(%error, "cannot fetch the weather");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answers_are_read_and_codes_described() {
        let body = json!({"current": {"temperature_2m": 7.5, "weather_code": 61},
                          "daily": {"temperature_2m_max": [9.0], "temperature_2m_min": [4.0]}});
        let weather = parse(&body, Some("Berlin".to_owned())).unwrap();
        assert_eq!(weather.summary, "Rain");
        assert_eq!(
            (weather.temperature_c, weather.high_c, weather.low_c),
            (Some(7.5), Some(9.0), Some(4.0))
        );
        assert!(parse(&json!({}), None).is_none());
        for (code, words) in [
            (0, "Clear sky"),
            (2, "Partly cloudy"),
            (45, "Fog"),
            (73, "Snow"),
            (96, "Thunderstorm"),
            (500, "Unknown"),
        ] {
            assert_eq!(describe(code), words);
        }
    }

    #[tokio::test]
    async fn the_weather_is_fetched_from_the_configured_server() {
        use axum::{Json, Router, routing::get};
        let app = Router::new().route(
            "/v1/forecast",
            get(|| async {
                Json(
                    json!({"current": {"temperature_2m": 12.0, "weather_code": 1},
                            "daily": {"temperature_2m_max": [15.0], "temperature_2m_min": [8.0]}}),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = WeatherConfig {
            latitude: 52.5,
            longitude: 13.4,
            location: None,
            base_url: Some(format!("http://{address}")),
        };
        let weather = today(&config).await.unwrap();
        assert_eq!(
            (weather.summary.as_str(), weather.high_c),
            ("Mainly clear", Some(15.0))
        );
        let unreachable = WeatherConfig {
            base_url: Some("http://127.0.0.1:9".to_owned()),
            ..config
        };
        assert!(today(&unreachable).await.is_none());
    }
}
