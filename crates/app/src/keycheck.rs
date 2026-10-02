//! "Test key": one tiny, free request that proves a key works.
//!
//! - AssemblyAI: `GET https://streaming.assemblyai.com/v3/token`, which
//!   mints a 60-second streaming token. It costs nothing (streaming bills
//!   by session time) and fails for a key without streaming access.
//! - Deepgram: `GET https://api.deepgram.com/v1/projects`, a free listing.
//!
//! The result is "works" or the service's own error text with its HTTP
//! status, e.g. `HTTP 404: Invalid API key`. The key is never logged.

use std::time::Duration;

use crate::config::Backend;

const TIMEOUT: Duration = Duration::from_secs(10);

pub fn check_key(backend: Backend, key: &str) -> Result<(), String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("no key entered".into());
    }
    let (url, auth) = match backend {
        Backend::AssemblyAi => (
            "https://streaming.assemblyai.com/v3/token?expires_in_seconds=60",
            key.to_string(),
        ),
        Backend::Deepgram => (
            "https://api.deepgram.com/v1/projects",
            format!("Token {key}"),
        ),
    };
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    match agent.get(url).set("Authorization", &auth).call() {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(describe_failure(code, &body))
        }
        Err(ureq::Error::Transport(t)) => Err(format!("could not reach the service: {t}")),
    }
}

/// `HTTP <status>: <the service's message>`, pulling the message out of
/// the JSON error shapes both services use.
pub fn describe_failure(status: u16, body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|j| {
            let pick = |k: &str| j.get(k).and_then(|v| v.as_str()).map(str::to_string);
            pick("detail")
                .or_else(|| pick("error"))
                .or_else(|| pick("message"))
                .or_else(|| pick("err_msg"))
                .or_else(|| {
                    // FastAPI validation errors: {"detail":[{"msg":"..."}]}
                    j.get("detail")?
                        .get(0)?
                        .get("msg")?
                        .as_str()
                        .map(str::to_string)
                })
        })
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    if message.is_empty() {
        format!("HTTP {status}")
    } else {
        format!("HTTP {status}: {message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assemblyai_bad_key_reads_as_its_message() {
        assert_eq!(
            describe_failure(404, r#"{"detail":"Invalid API key"}"#),
            "HTTP 404: Invalid API key"
        );
    }

    #[test]
    fn deepgram_bad_key_reads_as_its_message() {
        let body = r#"{"category":"UNAUTHORIZED","message":"Authentication failed.","details":"Check that you are using the correct credentials.","request_id":"x"}"#;
        assert_eq!(
            describe_failure(401, body),
            "HTTP 401: Authentication failed."
        );
    }

    #[test]
    fn validation_errors_and_plain_bodies() {
        let body = r#"{"detail":[{"type":"missing","loc":["header","authorization"],"msg":"Field required"}]}"#;
        assert_eq!(describe_failure(422, body), "HTTP 422: Field required");
        assert_eq!(
            describe_failure(500, "upstream down\n"),
            "HTTP 500: upstream down"
        );
        assert_eq!(describe_failure(502, ""), "HTTP 502");
    }

    #[test]
    fn empty_key_fails_without_a_request() {
        assert_eq!(
            check_key(Backend::Deepgram, "  "),
            Err("no key entered".into())
        );
    }

    #[test]
    #[ignore = "calls the real services; set ASSEMBLYAI_API_KEY and DEEPGRAM_API_KEY"]
    fn real_keys_work_and_a_bad_key_says_why() {
        for b in [Backend::AssemblyAi, Backend::Deepgram] {
            let key = std::env::var(b.key_var()).expect("key in env");
            assert_eq!(check_key(b, &key), Ok(()), "{b:?}");
            let err = check_key(b, "not-a-real-key").unwrap_err();
            assert!(err.starts_with("HTTP 4"), "{b:?}: {err}");
        }
    }
}
