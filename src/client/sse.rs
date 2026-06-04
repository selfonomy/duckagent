use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::blocking::Response;
use std::io::{BufRead, BufReader};

pub(crate) fn ensure_success_response(url: &str, response: Response) -> Result<Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }

    let body = response
        .text()
        .unwrap_or_else(|err| format!("<failed to read error body: {err}>"));
    bail!(
        "provider returned error for {url}: HTTP status {} ({})\nbody: {}",
        status.as_u16(),
        status
            .canonical_reason()
            .unwrap_or_else(|| status_reason_fallback(status)),
        body
    );
}

pub(crate) fn read_sse_events<F>(response: Response, mut on_event: F) -> Result<()>
where
    F: FnMut(&str) -> Result<()>,
{
    let reader = BufReader::new(response);
    let mut payload = String::new();

    for line in reader.lines() {
        let line = line.context("failed to read SSE line")?;
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            if !payload.trim().is_empty() {
                on_event(payload.trim())?;
                payload.clear();
            }
            continue;
        }
        if let Some(data) = trimmed.strip_prefix("data:") {
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(data.trim_start());
        }
    }

    if !payload.trim().is_empty() {
        on_event(payload.trim())?;
    }

    Ok(())
}

fn status_reason_fallback(status: StatusCode) -> &'static str {
    match status {
        StatusCode::BAD_REQUEST => "Bad Request",
        StatusCode::UNAUTHORIZED => "Unauthorized",
        StatusCode::FORBIDDEN => "Forbidden",
        StatusCode::NOT_FOUND => "Not Found",
        StatusCode::TOO_MANY_REQUESTS => "Too Many Requests",
        StatusCode::INTERNAL_SERVER_ERROR => "Internal Server Error",
        StatusCode::BAD_GATEWAY => "Bad Gateway",
        StatusCode::SERVICE_UNAVAILABLE => "Service Unavailable",
        StatusCode::GATEWAY_TIMEOUT => "Gateway Timeout",
        _ => "Unknown Status",
    }
}
