//! The handlers a route answers with, as axum's own `routing` functions
//! declare them, keeping the methods they answer, which an axum
//! [`MethodRouter`] does not tell.

use std::convert::Infallible;

use axum::extract::Request;
use axum::handler::Handler;
use axum::http::Method;
use axum::response::IntoResponse;
use axum::routing::{MethodRouter, Route};
use tower::{Layer, Service};

/// What one path answers, by method.
pub struct Methods<S> {
    pub(crate) methods: Vec<Method>,
    pub(crate) router: MethodRouter<S>,
}

impl<S: Clone + Send + Sync + 'static> Methods<S> {
    /// These methods with `layer` around each, as [`MethodRouter::layer`].
    pub fn layer<L>(self, layer: L) -> Self
    where
        L: Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: Service<Request> + Clone + Send + Sync + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        Self {
            methods: self.methods,
            router: self.router.layer(layer),
        }
    }

    fn and(self, method: Method, add: impl FnOnce(MethodRouter<S>) -> MethodRouter<S>) -> Self {
        Self {
            methods: self.methods.into_iter().chain([method]).collect(),
            router: add(self.router),
        }
    }
}

macro_rules! verbs {
    ($($verb:ident => $method:ident),* $(,)?) => {
        $(
            #[doc = concat!("`handler` answering `", stringify!($method), "`.")]
            pub fn $verb<H, T, S>(handler: H) -> Methods<S>
            where
                H: Handler<T, S>,
                T: 'static,
                S: Clone + Send + Sync + 'static,
            {
                Methods {
                    methods: vec![Method::$method],
                    router: axum::routing::$verb(handler),
                }
            }
        )*

        impl<S: Clone + Send + Sync + 'static> Methods<S> {
            $(
                #[doc = concat!("These methods, and `handler` answering `", stringify!($method), "`.")]
                pub fn $verb<H, T>(self, handler: H) -> Self
                where
                    H: Handler<T, S>,
                    T: 'static,
                {
                    self.and(Method::$method, |router| router.$verb(handler))
                }
            )*
        }
    };
}

verbs!(get => GET, post => POST, put => PUT, patch => PATCH, delete => DELETE);
