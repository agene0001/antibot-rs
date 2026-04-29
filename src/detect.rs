//! Cheap challenge detection — decide whether to invoke the (expensive) solver.
//!
//! The detection helpers are decoupled from the [`crate::Antibot`] client so they
//! can be used standalone. Construct a [`DetectionInput`] from whatever response
//! type you have (reqwest, hyper, etc.) and call [`detect_challenge`].

use http::HeaderMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChallengeKind {
    Cloudflare,
    DataDome,
    PerimeterX,
    Akamai,
    Imperva,
    Turnstile,
    Recaptcha,
    Unknown,
}

impl ChallengeKind {
    pub fn name(&self) -> &'static str {
        match self {
            ChallengeKind::Cloudflare => "cloudflare",
            ChallengeKind::DataDome => "datadome",
            ChallengeKind::PerimeterX => "perimeterx",
            ChallengeKind::Akamai => "akamai",
            ChallengeKind::Imperva => "imperva",
            ChallengeKind::Turnstile => "turnstile",
            ChallengeKind::Recaptcha => "recaptcha",
            ChallengeKind::Unknown => "unknown",
        }
    }
}

/// Slim view over an HTTP response, suitable for detection without owning it.
#[derive(Debug, Clone, Copy)]
pub struct DetectionInput<'a> {
    pub status: u16,
    pub headers: &'a HeaderMap,
    pub body: &'a str,
    pub url: &'a str,
}

impl<'a> DetectionInput<'a> {
    pub fn new(status: u16, headers: &'a HeaderMap, body: &'a str, url: &'a str) -> Self {
        Self { status, headers, body, url }
    }
}

/// Identify the challenge type, if any. Returns `None` when the response looks
/// clean.
///
/// Uses cheap header + body substring checks. Order matters: Cloudflare is
/// checked first because it's the most common and produces the fewest false
/// positives.
pub fn detect_challenge(input: &DetectionInput) -> Option<ChallengeKind> {
    if let Some(kind) = detect_cloudflare(input) {
        return Some(kind);
    }
    if detect_datadome(input) {
        return Some(ChallengeKind::DataDome);
    }
    if detect_perimeterx(input) {
        return Some(ChallengeKind::PerimeterX);
    }
    if detect_akamai(input) {
        return Some(ChallengeKind::Akamai);
    }
    if detect_imperva(input) {
        return Some(ChallengeKind::Imperva);
    }
    if detect_turnstile(input) {
        return Some(ChallengeKind::Turnstile);
    }
    if detect_recaptcha(input) {
        return Some(ChallengeKind::Recaptcha);
    }
    None
}

/// Same as [`detect_challenge`] but returns `Cloudflare` / `Turnstile` etc.
/// even when the page looks unblocked, useful for telemetry.
pub fn fingerprint(input: &DetectionInput) -> Vec<ChallengeKind> {
    let mut out = Vec::new();
    if detect_cloudflare(input).is_some() {
        out.push(ChallengeKind::Cloudflare);
    }
    if detect_datadome(input) {
        out.push(ChallengeKind::DataDome);
    }
    if detect_perimeterx(input) {
        out.push(ChallengeKind::PerimeterX);
    }
    if detect_akamai(input) {
        out.push(ChallengeKind::Akamai);
    }
    if detect_imperva(input) {
        out.push(ChallengeKind::Imperva);
    }
    if detect_turnstile(input) {
        out.push(ChallengeKind::Turnstile);
    }
    if detect_recaptcha(input) {
        out.push(ChallengeKind::Recaptcha);
    }
    out
}

fn detect_cloudflare(input: &DetectionInput) -> Option<ChallengeKind> {
    if input.headers.contains_key("cf-mitigated") {
        return Some(ChallengeKind::Cloudflare);
    }

    let cf_chl = input.headers.get("cf-chl-bypass").is_some();
    if cf_chl {
        return Some(ChallengeKind::Cloudflare);
    }

    let server_is_cloudflare = input
        .headers
        .get("server")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.eq_ignore_ascii_case("cloudflare"))
        .unwrap_or(false);

    let body = input.body;
    let body_signal = body.contains("Just a moment")
        || body.contains("__cf_chl_")
        || body.contains("/cdn-cgi/challenge-platform/")
        || body.contains("cf_chl_opt");

    if body_signal {
        return Some(ChallengeKind::Cloudflare);
    }

    if server_is_cloudflare && (input.status == 403 || input.status == 503) {
        return Some(ChallengeKind::Cloudflare);
    }

    None
}

fn detect_datadome(input: &DetectionInput) -> bool {
    let body = input.body;
    if body.contains("dd_cookie_test") || body.contains("geo.captcha-delivery.com") {
        return true;
    }

    let header_hit = input.headers.get_all("set-cookie").iter().any(|v| {
        v.to_str()
            .map(|s| s.contains("datadome="))
            .unwrap_or(false)
    });

    header_hit
}

fn detect_perimeterx(input: &DetectionInput) -> bool {
    let body = input.body;
    if body.contains("_pxhd") || body.contains("PerimeterX") || body.contains("/_px/") {
        return true;
    }

    input.headers.get_all("set-cookie").iter().any(|v| {
        v.to_str()
            .map(|s| s.contains("_px") || s.contains("_pxhd"))
            .unwrap_or(false)
    })
}

fn detect_akamai(input: &DetectionInput) -> bool {
    let body = input.body;
    if body.contains("ak_bmsc") || body.contains("akamaihd.net/sensor") {
        return true;
    }

    input.headers.get_all("set-cookie").iter().any(|v| {
        v.to_str()
            .map(|s| s.contains("ak_bmsc=") || s.contains("bm_sv="))
            .unwrap_or(false)
    })
}

fn detect_imperva(input: &DetectionInput) -> bool {
    let body = input.body;
    if body.contains("Incapsula") || body.contains("_Incapsula_Resource") {
        return true;
    }

    input
        .headers
        .get("x-iinfo")
        .or_else(|| input.headers.get("x-cdn"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("incap"))
        .unwrap_or(false)
}

fn detect_turnstile(input: &DetectionInput) -> bool {
    let body = input.body;
    body.contains("cf-turnstile") || body.contains("challenges.cloudflare.com/turnstile/")
}

fn detect_recaptcha(input: &DetectionInput) -> bool {
    let body = input.body;
    body.contains("www.google.com/recaptcha/")
        || body.contains("g-recaptcha")
        || body.contains("recaptcha/api.js")
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    fn input<'a>(status: u16, headers: &'a HeaderMap, body: &'a str) -> DetectionInput<'a> {
        DetectionInput::new(status, headers, body, "https://example.com/")
    }

    #[test]
    fn detects_cloudflare_just_a_moment() {
        let h = HeaderMap::new();
        let body = "<html><body>Just a moment...</body></html>";
        assert_eq!(
            detect_challenge(&input(403, &h, body)),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn detects_cf_mitigated_header() {
        let mut h = HeaderMap::new();
        h.insert("cf-mitigated", "challenge".parse().unwrap());
        assert_eq!(
            detect_challenge(&input(200, &h, "")),
            Some(ChallengeKind::Cloudflare)
        );
    }

    #[test]
    fn detects_datadome() {
        let h = HeaderMap::new();
        assert_eq!(
            detect_challenge(&input(403, &h, "var dd_cookie_test = true;")),
            Some(ChallengeKind::DataDome)
        );
    }

    #[test]
    fn detects_turnstile() {
        let h = HeaderMap::new();
        assert_eq!(
            detect_challenge(&input(200, &h, "<div class=\"cf-turnstile\"></div>")),
            Some(ChallengeKind::Turnstile)
        );
    }

    #[test]
    fn clean_response_returns_none() {
        let h = HeaderMap::new();
        assert_eq!(
            detect_challenge(&input(200, &h, "<html><body>hello</body></html>")),
            None
        );
    }
}
