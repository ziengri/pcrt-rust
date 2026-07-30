#![forbid(unsafe_code)]
//! Клиент одной попытки передачи результата в Passenger Flow API.
//!
//! Клиент не читает `SQLite` и не выполняет retry. Uploader определяет, когда
//! вызвать его повторно, используя классификацию [`DeliveryOutcome`].

use std::{env, time::Duration};

use reqwest::{
    StatusCode,
    blocking::{Client, ClientBuilder},
    header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue},
    redirect::Policy,
};
use serde::Deserialize;
use url::{ParseError, Url, form_urlencoded};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
const TIMELINE_PATH: &str = "/api/v1/timeline";
const X_AUTH_HEADER: HeaderName = HeaderName::from_static("x-auth");
const IDEMPOTENCY_KEY_HEADER: HeaderName = HeaderName::from_static("idempotency-key");

/// Конфигурация API-клиента без вывода секрета в логи.
pub struct ApiClientConfig {
    base_url: Url,
    x_auth: String,
    timeout: Duration,
}

impl ApiClientConfig {
    /// Создаёт конфигурацию API-клиента.
    ///
    /// Поддерживаются только `http` и `https` URL. Production-конфигурация
    /// должна использовать `https`; текущий совместимый API предоставляет `http`.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку для некорректного URL, пустого `X-AUTH` или нулевого
    /// timeout.
    pub fn new(
        base_url: impl AsRef<str>,
        x_auth: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ApiClientConfigError> {
        let base_url = Url::parse(base_url.as_ref()).map_err(ApiClientConfigError::InvalidUrl)?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(ApiClientConfigError::UnsupportedUrlScheme {
                scheme: base_url.scheme().to_owned(),
            });
        }
        if base_url.host_str().is_none() {
            return Err(ApiClientConfigError::MissingUrlHost);
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            return Err(ApiClientConfigError::BaseUrlContainsQueryOrFragment);
        }
        if timeout.is_zero() {
            return Err(ApiClientConfigError::ZeroTimeout);
        }

        let x_auth = x_auth.into();
        if x_auth.trim().is_empty() {
            return Err(ApiClientConfigError::EmptyXAuth);
        }

        Ok(Self {
            base_url,
            x_auth,
            timeout,
        })
    }

    /// Загружает `API_BASE_URL` и `API_X_AUTH` из environment.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку при отсутствии переменных или неверной конфигурации.
    pub fn from_environment() -> Result<Self, ApiClientConfigError> {
        let base_url =
            env::var("API_BASE_URL").map_err(|_| ApiClientConfigError::MissingEnvironment {
                name: "API_BASE_URL",
            })?;
        let x_auth = env::var("API_X_AUTH")
            .map_err(|_| ApiClientConfigError::MissingEnvironment { name: "API_X_AUTH" })?;
        Self::new(base_url, x_auth, DEFAULT_TIMEOUT)
    }

    /// Возвращает настроенный base URL без credentials.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base_url
    }
}

/// Ошибка инициализации API-клиента.
#[derive(Debug)]
pub enum ApiClientConfigError {
    InvalidUrl(ParseError),
    UnsupportedUrlScheme { scheme: String },
    MissingUrlHost,
    BaseUrlContainsQueryOrFragment,
    EmptyXAuth,
    ZeroTimeout,
    MissingEnvironment { name: &'static str },
    HttpClient(reqwest::Error),
}

impl core::fmt::Display for ApiClientConfigError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidUrl(error) => write!(formatter, "invalid API base URL: {error}"),
            Self::UnsupportedUrlScheme { scheme } => {
                write!(formatter, "unsupported API base URL scheme: {scheme}")
            }
            Self::MissingUrlHost => formatter.write_str("API base URL must contain a host"),
            Self::BaseUrlContainsQueryOrFragment => {
                formatter.write_str("API base URL must not contain a query or fragment")
            }
            Self::EmptyXAuth => formatter.write_str("API X-AUTH must not be empty"),
            Self::ZeroTimeout => formatter.write_str("API timeout must be greater than zero"),
            Self::MissingEnvironment { name } => {
                write!(
                    formatter,
                    "required API environment variable is missing: {name}"
                )
            }
            Self::HttpClient(error) => write!(formatter, "cannot create API HTTP client: {error}"),
        }
    }
}

impl std::error::Error for ApiClientConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidUrl(error) => Some(error),
            Self::HttpClient(error) => Some(error),
            Self::UnsupportedUrlScheme { .. }
            | Self::MissingUrlHost
            | Self::BaseUrlContainsQueryOrFragment
            | Self::EmptyXAuth
            | Self::ZeroTimeout
            | Self::MissingEnvironment { .. } => None,
        }
    }
}

/// Результат ровно одной попытки доставки.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeliveryOutcome {
    /// API подтвердил результат кодом `2xx`.
    Delivered,
    /// Uploader должен назначить следующую попытку.
    Retryable(DeliveryFailure),
    /// Uploader должен перенести сообщение в dead letter.
    Permanent(DeliveryFailure),
}

/// Причина неуспешной доставки без включения credentials или payload в текст.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryFailure {
    pub status: Option<u16>,
    pub message: String,
}

/// Одна попытка передачи timeline-результата.
///
/// `pcrt-uploader` зависит от этого узкого интерфейса, чтобы его retry policy
/// проверялась без реального HTTP-сервера.
pub trait TimelineDelivery {
    /// Передаёт один готовый результат и классифицирует итог попытки.
    fn send_timeline(&self, payload_json: &str, idempotency_key: &str) -> DeliveryOutcome;
}

/// HTTP-клиент результата пассажиропотока.
pub struct TimelineApiClient {
    client: Client,
    timeline_url: Url,
    x_auth: HeaderValue,
}

impl TimelineApiClient {
    /// Создаёт HTTP-клиент с timeout и отключёнными HTTP redirects.
    ///
    /// Redirect не считается успешной доставкой, потому что запрос мог быть
    /// перенаправлен на несовместимый endpoint.
    ///
    /// # Errors
    ///
    /// Возвращает ошибку, если невозможна настройка HTTP-клиента или заголовка
    /// `X-AUTH`.
    pub fn new(config: &ApiClientConfig) -> Result<Self, ApiClientConfigError> {
        let mut timeline_url = config.base_url.clone();
        timeline_url.set_path(TIMELINE_PATH);
        timeline_url.set_query(None);
        timeline_url.set_fragment(None);

        let x_auth =
            HeaderValue::from_str(&config.x_auth).map_err(|_| ApiClientConfigError::EmptyXAuth)?;
        let client = ClientBuilder::new()
            .timeout(config.timeout)
            .redirect(Policy::none())
            .build()
            .map_err(ApiClientConfigError::HttpClient)?;

        Ok(Self {
            client,
            timeline_url,
            x_auth,
        })
    }

    /// Выполняет одну попытку записи результата в `/api/v1/timeline`.
    ///
    /// `payload_json` должен соответствовать `contracts/api/timeline-v1.schema.json`.
    /// Клиент передаёт `Idempotency-Key`, но сервер должен сам реализовывать
    /// дедупликацию этого заголовка.
    #[must_use]
    pub fn send_timeline(&self, payload_json: &str, idempotency_key: &str) -> DeliveryOutcome {
        let payload = match TimelinePayload::parse(payload_json) {
            Ok(payload) => payload,
            Err(message) => return DeliveryOutcome::Permanent(DeliveryFailure::local(message)),
        };
        let idempotency_key = match HeaderValue::from_str(idempotency_key) {
            Ok(value) if !value.is_empty() => value,
            _ => {
                return DeliveryOutcome::Permanent(DeliveryFailure::local(
                    "Idempotency-Key must be a non-empty valid HTTP header value",
                ));
            }
        };

        let response = self
            .client
            .post(self.timeline_url.clone())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(X_AUTH_HEADER, self.x_auth.clone())
            .header(IDEMPOTENCY_KEY_HEADER, idempotency_key)
            .body(payload.form_body())
            .send();

        match response {
            Ok(response) => classify_status(response.status()),
            Err(error) => DeliveryOutcome::Retryable(DeliveryFailure {
                status: None,
                message: format!("API request failed: {error}"),
            }),
        }
    }
}

impl TimelineDelivery for TimelineApiClient {
    fn send_timeline(&self, payload_json: &str, idempotency_key: &str) -> DeliveryOutcome {
        TimelineApiClient::send_timeline(self, payload_json, idempotency_key)
    }
}

impl DeliveryFailure {
    fn local(message: impl Into<String>) -> Self {
        Self {
            status: None,
            message: message.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TimelinePayload {
    bus: String,
    cam: i64,
    date: String,
    #[serde(rename = "in")]
    passengers_in: i64,
    #[serde(rename = "out")]
    passengers_out: i64,
}

impl TimelinePayload {
    fn parse(payload_json: &str) -> Result<Self, String> {
        let payload = serde_json::from_str::<Self>(payload_json)
            .map_err(|error| format!("invalid timeline payload JSON: {error}"))?;
        if payload.bus.trim().is_empty() {
            return Err("timeline payload bus must not be empty".to_owned());
        }
        if payload.date.trim().is_empty() {
            return Err("timeline payload date must not be empty".to_owned());
        }
        Ok(payload)
    }

    fn form_body(&self) -> String {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        serializer.append_pair("bus", &self.bus);
        serializer.append_pair("cam", &self.cam.to_string());
        serializer.append_pair("date", &self.date);
        serializer.append_pair("in", &self.passengers_in.to_string());
        serializer.append_pair("out", &self.passengers_out.to_string());
        serializer.finish()
    }
}

fn classify_status(status: StatusCode) -> DeliveryOutcome {
    let failure = DeliveryFailure {
        status: Some(status.as_u16()),
        message: format!("Timeline API returned HTTP {status}"),
    };
    if status.is_success() {
        DeliveryOutcome::Delivered
    } else if matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error() {
        DeliveryOutcome::Retryable(failure)
    } else {
        DeliveryOutcome::Permanent(failure)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    use super::{
        ApiClientConfig, ApiClientConfigError, DeliveryOutcome, TimelineApiClient, TimelinePayload,
        classify_status,
    };
    use reqwest::StatusCode;

    #[test]
    fn payload_is_encoded_as_current_timeline_form_contract() {
        let payload = TimelinePayload::parse(
            r#"{"bus":"tst000","cam":1,"date":"29.07.2026T12:34","in":3,"out":1}"#,
        )
        .unwrap();

        assert_eq!(
            payload.form_body(),
            "bus=tst000&cam=1&date=29.07.2026T12%3A34&in=3&out=1"
        );
    }

    #[test]
    fn payload_rejects_unknown_and_empty_required_values() {
        assert!(TimelinePayload::parse(r#"{"bus":"","cam":1,"date":"x","in":0,"out":0}"#).is_err());
        assert!(
            TimelinePayload::parse(
                r#"{"bus":"tst000","cam":1,"date":"x","in":0,"out":0,"extra":true}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn api_statuses_are_classified_for_uploader() {
        assert_eq!(
            classify_status(StatusCode::CREATED),
            DeliveryOutcome::Delivered
        );
        assert!(matches!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            DeliveryOutcome::Retryable(_)
        ));
        assert!(matches!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            DeliveryOutcome::Retryable(_)
        ));
        assert!(matches!(
            classify_status(StatusCode::UNPROCESSABLE_ENTITY),
            DeliveryOutcome::Permanent(_)
        ));
    }

    #[test]
    fn client_sends_current_timeline_request_contract() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let bytes_read = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..bytes_read]);
                let headers_end = request.windows(4).position(|window| window == b"\r\n\r\n");
                if let Some(headers_end) = headers_end {
                    let headers = String::from_utf8_lossy(&request[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then_some(value.trim())
                        })
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    if request.len() >= headers_end + 4 + content_length {
                        break;
                    }
                }
            }
            stream
                .write_all(
                    b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let config = ApiClientConfig::new(
            format!("http://{address}/base-path"),
            "test-x-auth",
            Duration::from_secs(1),
        )
        .unwrap();
        let client = TimelineApiClient::new(&config).unwrap();
        let outcome = client.send_timeline(
            r#"{"bus":"tst000","cam":1,"date":"29.07.2026T12:34","in":3,"out":1}"#,
            "pcrt-result:test-session",
        );

        assert_eq!(outcome, DeliveryOutcome::Delivered);
        let request = server.join().unwrap().to_lowercase();
        assert!(request.starts_with("post /api/v1/timeline http/1.1\r\n"));
        assert!(request.contains("accept: application/json\r\n"));
        assert!(request.contains("content-type: application/x-www-form-urlencoded\r\n"));
        assert!(request.contains("x-auth: test-x-auth\r\n"));
        assert!(request.contains("idempotency-key: pcrt-result:test-session\r\n"));
        assert!(request.ends_with("bus=tst000&cam=1&date=29.07.2026t12%3a34&in=3&out=1"));
    }

    #[test]
    fn config_rejects_invalid_or_incomplete_values_without_rendering_secret() {
        assert!(matches!(
            ApiClientConfig::new("ftp://example.test", "secret", Duration::from_secs(1)),
            Err(ApiClientConfigError::UnsupportedUrlScheme { .. })
        ));
        assert!(matches!(
            ApiClientConfig::new("http://example.test", " ", Duration::from_secs(1)),
            Err(ApiClientConfigError::EmptyXAuth)
        ));
        assert!(matches!(
            ApiClientConfig::new("http://example.test", "secret", Duration::ZERO),
            Err(ApiClientConfigError::ZeroTimeout)
        ));
    }
}
