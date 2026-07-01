use std::sync::Arc;

use async_compat::CompatExt;
use futures::{AsyncReadExt, FutureExt, future::BoxFuture};
use gpui::http_client::{
    AsyncBody, HttpClient, Request, Response, Url,
    http::{HeaderValue, StatusCode},
};

const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

pub struct Client {
    client: reqwest::Client,
    user_agent: HeaderValue,
}

impl Client {
    pub fn new() -> Arc<Self> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("failed to build HTTP client");

        Arc::new(Self {
            client,
            user_agent: HeaderValue::from_static(USER_AGENT),
        })
    }
}

impl HttpClient for Client {
    fn user_agent(&self) -> Option<&HeaderValue> {
        Some(&self.user_agent)
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let (parts, body) = req.into_parts();
        let uri = parts.uri.to_string();
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let headers = parts.headers;
        let client = self.client.clone();

        async move {
            // Drain the request body (AsyncBody is smol-compatible, no bridging needed).
            let mut body = body;
            let mut body_bytes = Vec::new();
            body.read_to_end(&mut body_bytes).await?;

            let mut builder = client.request(method, &uri);
            for (name, value) in headers.iter() {
                builder = builder.header(name.as_str(), value.as_bytes());
            }
            if !body_bytes.is_empty() {
                builder = builder.body(body_bytes);
            }

            let response = builder.send().compat().await?;
            let status = response.status().as_u16();
            let bytes = response.bytes().compat().await?;

            let async_body = AsyncBody::from_bytes(bytes);
            let mut http_response = Response::new(async_body);
            *http_response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);

            Ok(http_response)
        }
        .boxed()
    }
}
