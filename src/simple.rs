use crate::check::TokenChecker;
use crate::error::ServiceError;
use crate::BoxFut;
use actix_service::Service;
use actix_service::Transform;
use actix_web::dev::ServiceRequest;
use actix_web::dev::ServiceResponse;
use actix_web::Error;
use biscuit::ValidationOptions;
use futures::future;
use futures::future::Ready;
use futures::task::Context;
use futures::task::Poll;
use futures::TryFutureExt;
use std::cell::RefCell;
use std::sync::Arc;

#[derive(Clone)]
pub struct SimpleAuth<T: TokenChecker + 'static> {
    pub checker: T,
    pub validation_options: ValidationOptions,
}
#[derive(Clone)]
pub struct SimpleAuthMiddleware<S, T: TokenChecker + 'static> {
    pub service: Arc<RefCell<S>>,
    pub checker: Arc<T>,
    pub validation_options: ValidationOptions,
}

impl<S, B: 'static, T: TokenChecker + Clone + 'static> Transform<S> for SimpleAuth<T>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = SimpleAuthMiddleware<S, T>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(SimpleAuthMiddleware {
            service: Arc::new(RefCell::new(service)),
            checker: Arc::new(self.checker.clone()),
            validation_options: self.validation_options.clone(),
        })
    }
}

impl<S, B, T: TokenChecker + 'static> Service for SimpleAuthMiddleware<S, T>
where
    S: Service<Request = ServiceRequest, Response = ServiceResponse<B>, Error = Error> + 'static,
    S::Future: 'static,
    B: 'static,
{
    type Request = ServiceRequest;
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = BoxFut<Self::Response, Self::Error>;

    fn poll_ready(&mut self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        (*self.service).borrow_mut().poll_ready(cx)
    }

    fn call(&mut self, req: ServiceRequest) -> Self::Future {
        if req.method() == "OPTIONS" {
            return Box::pin(self.service.borrow_mut().call(req));
        }

        let auth_header = match req.headers().get("AUTHORIZATION") {
            Some(value) => value.to_str().ok(),
            None => return Box::pin(future::err(ServiceError::Unauthorized.into())),
        };

        if let Some(auth_header) = auth_header {
            if let Some(token) = get_token(auth_header) {
                let svc = self.service.clone();
                let validation_options = self.validation_options.clone();
                let fut = self.checker.verify_and_decode(token.to_owned());
                return Box::pin(async move {
                    let claim_set = fut.map_err(Error::from).await?;
                    match T::check(&claim_set, validation_options) {
                        Ok(_) => svc.borrow_mut().call(req).await,
                        Err(_) => Err(ServiceError::Unauthorized.into()),
                    }
                });
            }
        }
        Box::pin(future::err(ServiceError::Unauthorized.into()))
    }
}

fn get_token(auth_header: &str) -> Option<&str> {
    match auth_header.get(0..7) {
        Some("Bearer ") => auth_header.get(7..),
        _ => None,
    }
}
#[cfg(test)]
mod test {
    use super::*;
    use actix_service::IntoService;
    use actix_web::test::TestRequest;
    use actix_web::HttpResponse;
    use biscuit::ClaimsSet;
    use failure::Error;
    use futures::future::ok;
    use futures::future::BoxFuture;
    use serde_json::Value;

    #[derive(Default, Clone)]
    struct FakeChecker {
        pub claim_set: Option<ClaimsSet<Value>>,
        pub token: Option<String>,
    }

    impl TokenChecker for FakeChecker {
        type Item = biscuit::ClaimsSet<Value>;
        type Future = BoxFuture<'static, Result<Self::Item, Error>>;
        fn verify_and_decode(&self, token: String) -> Self::Future {
            if let Some(cs) = &self.claim_set {
                if self
                    .token
                    .as_ref()
                    .map(|t| t == &token)
                    .unwrap_or_else(|| true)
                {
                    return Box::pin(future::ok(cs.clone()));
                }
            };
            Box::pin(future::err(ServiceError::Unauthorized.into()))
        }
        fn check(item: &Self::Item, validation_options: ValidationOptions) -> Result<(), Error> {
            item.registered
                .validate(validation_options)
                .map_err(Into::into)
        }
    }

    #[test]
    fn test_get_token() {
        let token = "Bearer FOOBAR…";
        assert_eq!(get_token(token), Some("FOOBAR…"));
    }

    #[actix_rt::test]
    async fn test_middleware_no_token() {
        let srv = |req: ServiceRequest| ok(req.into_response(HttpResponse::Ok()));
        let auth_middleware = SimpleAuth {
            checker: FakeChecker::default(),
            validation_options: ValidationOptions::default(),
        };
        let mut srv = auth_middleware
            .new_transform(srv.into_service())
            .await
            .unwrap();
        let req = TestRequest::with_header("SOMETHING", "ELSE").to_srv_request();
        let res = srv.call(req).await;
        assert!(res.is_err());
    }

    #[actix_rt::test]
    async fn test_middleware_bearer_missing() {
        let srv = |req: ServiceRequest| ok(req.into_response(HttpResponse::Ok()));
        let auth_middleware = SimpleAuth {
            checker: FakeChecker {
                claim_set: Some(ClaimsSet {
                    registered: Default::default(),
                    private: Value::default(),
                }),
                token: None,
            },
            validation_options: ValidationOptions::default(),
        };
        let mut srv = auth_middleware
            .new_transform(srv.into_service())
            .await
            .unwrap();
        let req = TestRequest::with_header("AUTHORIZATION", "not bearer").to_srv_request();
        let res = srv.call(req).await;
        assert!(res.is_err());
    }

    #[actix_rt::test]
    async fn test_middleware_authorized() {
        let srv = |req: ServiceRequest| ok(req.into_response(HttpResponse::Ok()));
        let auth_middleware = SimpleAuth {
            checker: FakeChecker {
                claim_set: Some(ClaimsSet {
                    registered: Default::default(),
                    private: Value::default(),
                }),
                token: None,
            },
            validation_options: ValidationOptions::default(),
        };
        let mut srv = auth_middleware
            .new_transform(srv.into_service())
            .await
            .unwrap();
        let req = TestRequest::with_header("AUTHORIZATION", "Bearer somethingfun").to_srv_request();
        let res = srv.call(req).await;
        assert!(res.is_ok());
    }
}
