use crate::api::{HttpRequest, HttpResponse, raw_response};

pub(crate) fn route(request: &HttpRequest) -> Option<HttpResponse> {
    if request.method != "GET" {
        return None;
    }

    let path = request
        .target
        .split_once('?')
        .map_or(request.target.as_str(), |(path, _)| path);

    Some(match path {
        "/dashboard" | "/dashboard/" => {
            let mut response = asset_response(
                "text/html; charset=utf-8",
                include_bytes!("../assets/dashboard.html").to_vec(),
            );
            response.headers.push((
                "Content-Security-Policy".to_string(),
                "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"
                    .to_string(),
            ));
            response.headers.push((
                "Referrer-Policy".to_string(),
                "no-referrer".to_string(),
            ));
            response
        }
        "/dashboard/app.js" => asset_response(
            "application/javascript; charset=utf-8",
            include_bytes!("../assets/dashboard.js").to_vec(),
        ),
        "/dashboard/style.css" => asset_response(
            "text/css; charset=utf-8",
            include_bytes!("../assets/dashboard.css").to_vec(),
        ),
        _ => return None,
    })
}

fn asset_response(content_type: &'static str, body: Vec<u8>) -> HttpResponse {
    let mut response = raw_response(200, content_type, body);
    response.headers.push((
        "Cross-Origin-Resource-Policy".to_string(),
        "same-origin".to_string(),
    ));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn request(target: &str) -> HttpRequest {
        HttpRequest {
            method: "GET".to_string(),
            target: target.to_string(),
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn dashboard_has_no_remote_runtime_dependencies() {
        let page = String::from_utf8_lossy(include_bytes!("../assets/dashboard.html"));
        let script = String::from_utf8_lossy(include_bytes!("../assets/dashboard.js"));
        let style = String::from_utf8_lossy(include_bytes!("../assets/dashboard.css"));

        for asset in [&page, &script, &style] {
            assert!(!asset.contains("http://"));
            assert!(!asset.contains("https://"));
            assert!(!asset.contains("//cdn."));
        }
    }

    #[test]
    fn dashboard_assets_are_self_hosted() {
        let page = route(&request("/dashboard")).expect("dashboard route");
        assert_eq!(page.status, 200);
        assert_eq!(page.content_type, "text/html; charset=utf-8");
        assert!(String::from_utf8_lossy(&page.body).contains("/dashboard/app.js"));
        assert!(page.headers.iter().any(|(name, value)| {
            name == "Content-Security-Policy" && value.contains("script-src 'self'")
        }));

        let js = route(&request("/dashboard/app.js")).expect("dashboard JS");
        assert_eq!(js.content_type, "application/javascript; charset=utf-8");

        let css = route(&request("/dashboard/style.css")).expect("dashboard CSS");
        assert_eq!(css.content_type, "text/css; charset=utf-8");
    }
}
