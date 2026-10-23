//! HTTP security hardening middleware and utilities.

use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::Response;

/// Content-Security-Policy applied to every gateway response.
///
/// Trade-offs honed against the actual runtime:
///   * `style-src 'unsafe-inline'` — Tailwind utility classes and Radix
///     primitives emit inline style attributes; no way around it.
///   * `worker-src 'self' blob:` — Monaco editor (self-hosted via
///     monaco-setup.ts) spawns its language-service workers from
///     Vite-emitted same-origin chunks; `blob:` covers dev HMR.
///   * `script-src 'self' 'unsafe-eval'` — Monaco's tokenizer / Monarch
///     grammar engine relies on `new Function()` at runtime. Acceptable
///     because Monaco is bundled from npm and our SRI hashes guarantee
///     the bundle wasn't tampered with in transit.
///   * `connect-src 'self' wss:` — WebSocket gateway path.
const CSP: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-eval'; \
    worker-src 'self' blob:; \
    connect-src 'self' wss:; \
    img-src 'self' data: blob:; \
    style-src 'self' 'unsafe-inline'; \
    font-src 'self' data:; \
    object-src 'none'; \
    base-uri 'self'; \
    form-action 'self'; \
    frame-ancestors 'none'";

/// Permissions-Policy: deny every powerful capability by default. The
/// dashboard doesn't need camera/mic/geolocation/payment/USB; this stops
/// any injected content from using them.
const PERMISSIONS_POLICY: &str = "accelerometer=(), camera=(), geolocation=(), \
    gyroscope=(), magnetometer=(), microphone=(), payment=(), \
    serial=(), usb=(), interest-cohort=()";

/// Middleware that adds security headers to all HTTP responses.
pub async fn security_headers_middleware(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert(
        "Strict-Transport-Security",
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    headers.insert("X-Frame-Options", HeaderValue::from_static("DENY"));
    headers.insert(
        "X-Content-Type-Options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("Content-Security-Policy", HeaderValue::from_static(CSP));
    headers.insert("X-XSS-Protection", HeaderValue::from_static("0"));
    headers.insert(
        "Referrer-Policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "Permissions-Policy",
        HeaderValue::from_static(PERMISSIONS_POLICY),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    async fn dummy_handler() -> &'static str {
        "ok"
    }

    #[tokio::test]
    async fn adds_security_headers() {
        let app = Router::new()
            .route("/test", get(dummy_handler))
            .layer(axum::middleware::from_fn(security_headers_middleware));

        let response = app
            .oneshot(Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("Strict-Transport-Security").unwrap(),
            "max-age=31536000; includeSubDomains"
        );
        assert_eq!(response.headers().get("X-Frame-Options").unwrap(), "DENY");
        assert_eq!(
            response.headers().get("X-Content-Type-Options").unwrap(),
            "nosniff"
        );
        let csp = response
            .headers()
            .get("Content-Security-Policy")
            .unwrap()
            .to_str()
            .unwrap();
        // Pin the load-bearing directives — full string is whitespace-sensitive.
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(csp.contains("script-src 'self'"));
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("object-src 'none'"));
        assert_eq!(
            response.headers().get("Referrer-Policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
        let pp = response
            .headers()
            .get("Permissions-Policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(pp.contains("camera=()"));
        assert!(pp.contains("microphone=()"));
        assert!(pp.contains("geolocation=()"));
    }
}
