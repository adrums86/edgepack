use crate::config::{DrmConfig, SpekeAuth};
use crate::drm::cpix;
use crate::drm::system_ids;
use crate::drm::DrmKeySet;
use crate::error::{EdgePackagerError, Result};

/// SPEKE 2.0 client for communicating with a DRM license/key server.
///
/// Implements the Secure Packager and Encoder Key Exchange (SPEKE) protocol
/// version 2.0, which uses CPIX documents over HTTP POST.
pub struct SpekeClient {
    endpoint: String,
    auth: SpekeAuth,
    system_ids: Vec<[u8; 16]>,
}

impl SpekeClient {
    pub fn new(config: &DrmConfig) -> Self {
        let mut ids = Vec::new();
        if config.system_ids.widevine {
            ids.push(system_ids::WIDEVINE);
        }
        if config.system_ids.playready {
            ids.push(system_ids::PLAYREADY);
        }

        Self {
            endpoint: config.speke_url.to_string(),
            auth: config.speke_auth.clone(),
            system_ids: ids,
        }
    }

    /// Request content keys from the SPEKE 2.0 server.
    ///
    /// Builds a CPIX request document, POSTs it to the license server,
    /// and parses the CPIX response to extract content keys and DRM data.
    pub fn request_keys(
        &self,
        content_id: &str,
        key_ids: &[[u8; 16]],
    ) -> Result<DrmKeySet> {
        // Build the CPIX request XML
        let request_body = cpix::build_cpix_request(content_id, key_ids, &self.system_ids)?;

        // Make the HTTP POST request to the SPEKE endpoint
        let response_body = self.post_cpix(&request_body)?;

        // Parse the CPIX response
        cpix::parse_cpix_response(response_body.as_bytes())
    }

    /// POST a CPIX document to the SPEKE endpoint.
    ///
    /// In a WASI environment, this uses `wasi:http/outgoing-handler`.
    fn post_cpix(&self, body: &str) -> Result<String> {
        // Build HTTP request:
        // POST {endpoint}
        // Content-Type: application/xml
        // Authorization: Bearer {token}  (or appropriate auth header)
        //
        // {CPIX XML body}

        let _endpoint = &self.endpoint;
        let _content_type = "application/xml";
        let _auth_header = self.build_auth_header();
        let _body = body;

        // TODO: Implement using wasi:http/outgoing-handler
        //
        // 1. Create outgoing HTTP request to SPEKE endpoint
        // 2. Set Content-Type: application/xml
        // 3. Set auth header (Bearer, API key, or Basic)
        // 4. Set body to CPIX XML
        // 5. Send via wasi:http/outgoing-handler
        // 6. Read response body (expect 200 with CPIX XML)
        // 7. Return response body as string

        Err(EdgePackagerError::Speke(
            "WASI HTTP transport not yet implemented".into(),
        ))
    }

    fn build_auth_header(&self) -> (String, String) {
        match &self.auth {
            SpekeAuth::Bearer(token) => {
                ("Authorization".to_string(), format!("Bearer {token}"))
            }
            SpekeAuth::ApiKey { header, value } => {
                (header.clone(), value.clone())
            }
            SpekeAuth::Basic { username, password } => {
                let credentials = base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    format!("{username}:{password}"),
                );
                ("Authorization".to_string(), format!("Basic {credentials}"))
            }
        }
    }
}
