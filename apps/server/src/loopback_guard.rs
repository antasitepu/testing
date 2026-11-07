//! The server only binds to loopback, but a browser can still be pointed at it by a
//! foreign page: through DNS rebinding (foreign `Host`) or a cross-site form/fetch
//! (foreign `Origin` on a state-changing request). Requests without these headers
//! do not come from a browser page and are allowed. A deployment behind its own reverse
//! proxy may name one public host, which is then accepted alongside loopback.
use crate::dto::ApiErrorDto;
use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use std::{net::IpAddr, sync::Arc};

pub async fn require_loopback_client(
    State(public_host): State<Option<Arc<str>>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(rejection) = rejection(request.method(), request.headers(), public_host.as_deref())
    {
        return rejection;
    }
    next.run(request).await
}

fn rejection(method: &Method, headers: &HeaderMap, public_host: Option<&str>) -> Option<Response> {
    if let Some(host) = headers.get(header::HOST)
        && !host
            .to_str()
            .is_ok_and(|host| is_loopback_authority(host) || is_public_host(host, public_host))
    {
        return Some(reject(
            StatusCode::MISDIRECTED_REQUEST,
            "NON_LOOPBACK_HOST",
            "requests must address a loopback host",
        ));
    }
    let safe = matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if !safe
        && let Some(origin) = headers.get(header::ORIGIN)
        && !origin
            .to_str()
            .is_ok_and(|origin| is_loopback_origin(origin) || is_public_origin(origin, public_host))
    {
        return Some(reject(
            StatusCode::FORBIDDEN,
            "CROSS_ORIGIN_REQUEST",
            "state-changing requests must come from a loopback page",
        ));
    }
    None
}

fn reject(status: StatusCode, code: &'static str, message: &str) -> Response {
    (
        status,
        Json(ApiErrorDto {
            code,
            message: message.to_owned(),
        }),
    )
        .into_response()
}

fn is_public_host(host: &str, public_host: Option<&str>) -> bool {
    public_host.is_some_and(|public_host| host.eq_ignore_ascii_case(public_host))
}

fn is_public_origin(origin: &str, public_host: Option<&str>) -> bool {
    origin
        .strip_prefix("https://")
        .is_some_and(|host| is_public_host(host, public_host))
}

fn is_loopback_origin(origin: &str) -> bool {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(is_loopback_authority)
}

fn is_loopback_authority(authority: &str) -> bool {
    let host = if let Some(bracketed) = authority.strip_prefix('[') {
        bracketed.split_once(']').map_or("", |(host, _)| host)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if port.bytes().all(|byte| byte.is_ascii_digit()) => host,
            _ => authority,
        }
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::rejection;
    use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        headers
    }

    fn status(method: Method, pairs: &[(header::HeaderName, &str)]) -> Option<StatusCode> {
        public_status(method, pairs, None)
    }

    fn public_status(
        method: Method,
        pairs: &[(header::HeaderName, &str)],
        public_host: Option<&str>,
    ) -> Option<StatusCode> {
        rejection(&method, &headers(pairs), public_host).map(|response| response.status())
    }

    #[test]
    fn loopback_hosts_and_headerless_clients_are_allowed() {
        for host in [
            "127.0.0.1:3000",
            "localhost:5173",
            "[::1]:3000",
            "LOCALHOST",
        ] {
            assert_eq!(status(Method::GET, &[(header::HOST, host)]), None, "{host}");
        }
        assert_eq!(status(Method::POST, &[]), None);
    }

    #[test]
    fn rebinding_host_is_rejected() {
        for host in [
            "attacker.example:3000",
            "localhost.attacker.example",
            "10.0.0.5:3000",
        ] {
            assert_eq!(
                status(Method::GET, &[(header::HOST, host)]),
                Some(StatusCode::MISDIRECTED_REQUEST),
                "{host}"
            );
        }
    }

    #[test]
    fn only_the_named_public_host_is_accepted_besides_loopback() {
        let public = Some("trade.example.com");
        for host in ["trade.example.com", "TRADE.example.com", "127.0.0.1:3000"] {
            assert_eq!(
                public_status(Method::GET, &[(header::HOST, host)], public),
                None,
                "{host}"
            );
        }
        for host in [
            "attacker.example",
            "trade.example.com.attacker.example",
            "other.example.com",
            "trade.example.com:8453",
        ] {
            assert_eq!(
                public_status(Method::GET, &[(header::HOST, host)], public),
                Some(StatusCode::MISDIRECTED_REQUEST),
                "{host}"
            );
        }
        assert_eq!(
            status(Method::GET, &[(header::HOST, "trade.example.com")]),
            Some(StatusCode::MISDIRECTED_REQUEST),
            "no public host is accepted unless one is configured"
        );
    }

    #[test]
    fn state_changes_are_accepted_only_from_the_https_public_origin() {
        let public = Some("trade.example.com");
        let host = (header::HOST, "trade.example.com");
        assert_eq!(
            public_status(
                Method::POST,
                &[host.clone(), (header::ORIGIN, "https://trade.example.com")],
                public
            ),
            None
        );
        for origin in [
            "http://trade.example.com",
            "https://trade.example.com.attacker.example",
            "https://attacker.example",
            "null",
        ] {
            assert_eq!(
                public_status(
                    Method::POST,
                    &[host.clone(), (header::ORIGIN, origin)],
                    public
                ),
                Some(StatusCode::FORBIDDEN),
                "{origin}"
            );
        }
    }

    #[test]
    fn cross_origin_state_change_is_rejected_but_reads_are_not() {
        let foreign = [
            (header::HOST, "127.0.0.1:3000"),
            (header::ORIGIN, "https://attacker.example"),
        ];
        assert_eq!(status(Method::POST, &foreign), Some(StatusCode::FORBIDDEN));
        assert_eq!(
            status(Method::POST, &[(header::ORIGIN, "null")]),
            Some(StatusCode::FORBIDDEN)
        );
        assert_eq!(status(Method::GET, &foreign), None);
        assert_eq!(
            status(
                Method::POST,
                &[
                    (header::HOST, "127.0.0.1:5173"),
                    (header::ORIGIN, "http://127.0.0.1:5173"),
                ],
            ),
            None
        );
    }
}
