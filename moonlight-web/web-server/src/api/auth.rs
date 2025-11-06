use actix_web::{
    Error, HttpResponse,
    body::{BoxBody, MessageBody},
    dev::{ServiceRequest, ServiceResponse},
    http::header,
    middleware::Next,
    web::Data,
};
use log::{error, warn};

#[derive(Clone)]
pub struct ApiCredentials {
    pub credentials: Option<String>,
}

pub async fn auth_middleware(
    req: ServiceRequest,
    next: Next<BoxBody>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    if req.uri().path() == "/api/host/stream" {
        // This will route the stream web socket through
        // because web socket cannot have the auth header
        // The Ws is authenticated in the start_stream handler
        return next.call(req).await;
    }

    let Some(credentials) = req.app_data::<Data<ApiCredentials>>() else {
        let response = HttpResponse::InternalServerError().finish();

        error!("No ApiCredentials present in the app. Cannot verify request -> blocking request.");

        return Ok(req.into_response(response));
    };

    let authenticated = match credentials.authenticate(&req) {
        Err(err) => {
            return Ok(req.into_response(err));
        }
        Ok(value) => value,
    };

    if authenticated {
        next.call(req).await
    } else {
        let response = HttpResponse::Unauthorized().finish();

        Ok(req.into_response(response))
    }
}

impl ApiCredentials {
    fn authenticate(&self, request: &ServiceRequest) -> Result<bool, HttpResponse> {
        let Some(value) = request
            .head()
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
        else {
            return Ok(self.authenticate_with_credentials(None));
        };

        let Some((auth_type, request_credentials)) = value.split_once(" ") else {
            warn!("[Auth] Received malformed Authorization header!");
            return Err(HttpResponse::BadRequest().finish());
        };

        Ok(auth_type == "Bearer" && self.authenticate_with_credentials(Some(request_credentials)))
    }

    pub fn enable_credential_authentication(&self) -> bool {
        self.credentials.is_some()
    }
    pub fn authenticate_with_credentials(&self, request_credentials: Option<&str>) -> bool {
        let Some(credentials) = self.credentials.as_ref() else {
            // This is the case when no credentials / auth is requested by the user
            // -> allow the request

            return true;
        };

        request_credentials == Some(credentials)
    }
}
