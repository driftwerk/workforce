//! Every route a hub serves, declared with its class, and rendered for the
//! surface a listener serves: the member surface never carries an admin
//! route, and an admin route elsewhere is behind the admin guard before its
//! handler runs.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use axum::extract::Request;
use axum::handler::Handler;
use axum::http::{Method, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{MethodFilter, MethodRouter, Route};
use axum::Router;
use tower::{Layer, Service};

use crate::routing::Methods;
use crate::Surface;

/// Who a route is for.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum RouteClass {
    /// Needs no credential.
    Open,
    /// For anyone signed in; it narrows itself to what the caller may see.
    Member,
    /// Only for those beyond a member: administrators, managers and custom
    /// roles.
    Admin,
    /// Checks a secret of its own, not a person's credential.
    Machine,
}

impl RouteClass {
    /// Whether a listener of `surface` serves routes of this class.
    pub fn served_on(self, surface: Surface) -> bool {
        !(self == Self::Admin && surface == Surface::Public)
    }

    fn name(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Member => "member",
            Self::Admin => "admin",
            Self::Machine => "machine",
        }
    }
}

impl fmt::Display for RouteClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One method of one declared route.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Declared {
    pub method: Method,
    pub path: String,
    pub class: RouteClass,
}

impl fmt::Display for Declared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.method, self.path, self.class)
    }
}

type Wrap = Arc<dyn Fn(MethodRouter) -> MethodRouter + Send + Sync>;

/// A method router and the layers [`Routes::layer`] put around it, which
/// are applied as it is rendered, outside the admin guard.
struct Layered<S> {
    router: MethodRouter<S>,
    wraps: Vec<Wrap>,
}

impl<S: Clone + Send + Sync + 'static> Layered<S> {
    fn with_state(self, state: S) -> Layered<()> {
        Layered {
            router: self.router.with_state(state),
            wraps: self.wraps,
        }
    }
}

impl Layered<()> {
    fn wrapped(mut self, wrap: &Wrap) -> Self {
        self.wraps.push(Arc::clone(wrap));
        self
    }

    fn finish(self) -> MethodRouter {
        self.wraps
            .iter()
            .fold(self.router, |router, wrap| wrap(router))
    }
}

struct Entry<S> {
    path: Cow<'static, str>,
    class: RouteClass,
    methods: Vec<Method>,
    served: Layered<S>,
}

/// What a part of a hub's routes answers with: its state, or nothing when
/// only what the routes declare is read, as the manifest of a hub whose
/// parts are not started reads them.
#[derive(Clone, Copy, Debug)]
pub enum Bound<S> {
    To(S),
    Declared,
}

impl<S> Bound<S> {
    pub fn map<T>(self, f: impl FnOnce(S) -> T) -> Bound<T> {
        match self {
            Self::To(state) => Bound::To(f(state)),
            Self::Declared => Bound::Declared,
        }
    }

    pub fn as_ref(&self) -> Bound<&S> {
        match self {
            Self::To(state) => Bound::To(state),
            Self::Declared => Bound::Declared,
        }
    }
}

/// The routes of a hub, each with its [`RouteClass`]: the only way a route
/// reaches a listener.
pub struct Routes<S = ()> {
    entries: Vec<Entry<S>>,
    fallback: Option<Layered<S>>,
}

impl<S> Default for Routes<S> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            fallback: None,
        }
    }
}

impl<S: Clone + Send + Sync + 'static> Routes<S> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(self, path: &'static str, methods: Methods<S>) -> Self {
        self.declare(RouteClass::Open, path, methods)
    }

    pub fn member(self, path: &'static str, methods: Methods<S>) -> Self {
        self.declare(RouteClass::Member, path, methods)
    }

    pub fn admin(self, path: &'static str, methods: Methods<S>) -> Self {
        self.declare(RouteClass::Admin, path, methods)
    }

    pub fn machine(self, path: &'static str, methods: Methods<S>) -> Self {
        self.declare(RouteClass::Machine, path, methods)
    }

    fn declare(mut self, class: RouteClass, path: &'static str, methods: Methods<S>) -> Self {
        self.entries.push(Entry {
            path: Cow::Borrowed(path),
            class,
            methods: methods.methods,
            served: Layered {
                router: methods.router,
                wraps: Vec::new(),
            },
        });
        self
    }

    /// These routes and `other`'s. Like [`Router::merge`], two fallbacks
    /// cannot be merged.
    pub fn merge(mut self, other: Routes<S>) -> Self {
        self.fallback = match (self.fallback, other.fallback) {
            (Some(_), Some(_)) => panic!("cannot merge two `Routes` that both have a fallback"),
            (ours, theirs) => ours.or(theirs),
        };
        self.entries.extend(other.entries);
        self
    }

    /// These routes and `other`'s under `prefix`; `other` has no fallback
    /// of its own, as one would answer for every path under `prefix`.
    pub fn nest(mut self, prefix: &'static str, other: Routes<S>) -> Self {
        assert!(
            other.fallback.is_none(),
            "cannot nest `Routes` that have a fallback"
        );
        self.entries
            .extend(other.entries.into_iter().map(|entry| Entry {
                path: Cow::Owned(match entry.path.as_ref() {
                    "/" => prefix.to_string(),
                    path => format!("{prefix}{path}"),
                }),
                ..entry
            }));
        self
    }

    /// What answers a path no route declares, as [`Router::fallback`].
    pub fn fallback<H, T>(mut self, handler: H) -> Self
    where
        H: Handler<T, S>,
        T: 'static,
    {
        assert!(self.fallback.is_none(), "`Routes` already have a fallback");
        self.fallback = Some(Layered {
            router: MethodRouter::new().fallback(handler),
            wraps: Vec::new(),
        });
        self
    }

    /// These routes with `layer` around each of their handlers, inside every
    /// other layer, as [`Router::route_layer`].
    pub fn route_layer<L>(self, layer: L) -> Self
    where
        L: Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: Service<Request, Error = Infallible> + Clone + Send + Sync + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        Self {
            entries: self
                .entries
                .into_iter()
                .map(|entry| Entry {
                    served: Layered {
                        router: entry.served.router.route_layer(layer.clone()),
                        wraps: entry.served.wraps,
                    },
                    ..entry
                })
                .collect(),
            fallback: self.fallback,
        }
    }

    pub fn with_state(self, state: S) -> Routes<()> {
        Routes {
            entries: self
                .entries
                .into_iter()
                .map(|entry| Entry {
                    path: entry.path,
                    class: entry.class,
                    methods: entry.methods,
                    served: entry.served.with_state(state.clone()),
                })
                .collect(),
            fallback: self.fallback.map(|fallback| fallback.with_state(state)),
        }
    }

    /// These routes answering with `bound`'s state; declared alone, each
    /// answers its methods with 405, behind the same layers and guard, and
    /// they have no fallback.
    pub fn bind(self, bound: Bound<S>) -> Routes<()> {
        match bound {
            Bound::To(state) => self.with_state(state),
            Bound::Declared => Routes {
                entries: self
                    .entries
                    .into_iter()
                    .map(|entry| Entry {
                        served: Layered {
                            router: unanswered(&entry.methods),
                            wraps: Vec::new(),
                        },
                        path: entry.path,
                        class: entry.class,
                        methods: entry.methods,
                    })
                    .collect(),
                fallback: None,
            },
        }
    }

    /// Every method of every route, with its class, ordered by path and
    /// method.
    pub fn manifest(&self) -> Vec<Declared> {
        let mut declared: Vec<Declared> = self
            .entries
            .iter()
            .flat_map(|entry| {
                entry.methods.iter().map(|method| Declared {
                    method: method.clone(),
                    path: entry.path.to_string(),
                    class: entry.class,
                })
            })
            .collect();
        declared.sort_by(|a, b| (&a.path, a.method.as_str()).cmp(&(&b.path, b.method.as_str())));
        declared
    }
}

impl Routes<()> {
    /// These routes and their fallback with `layer` around them, as
    /// [`Router::layer`]: routes merged in later are left out of it.
    pub fn layer<L>(self, layer: L) -> Self
    where
        L: Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: Service<Request> + Clone + Send + Sync + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Error: Into<Infallible> + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        let wrap: Wrap = Arc::new(move |router: MethodRouter| router.layer(layer.clone()));
        Self {
            entries: self
                .entries
                .into_iter()
                .map(|entry| Entry {
                    served: entry.served.wrapped(&wrap),
                    ..entry
                })
                .collect(),
            fallback: self.fallback.map(|fallback| fallback.wrapped(&wrap)),
        }
    }

    /// The router a listener of `surface` serves. The member surface leaves
    /// out every admin route: a path or method only an admin route declares
    /// is answered by the fallback, as an unknown one is, and never by a
    /// sibling route that would otherwise match it. Elsewhere each admin
    /// route has `admin_guard` in front of its handler.
    #[allow(clippy::disallowed_methods)]
    pub fn render<L>(self, surface: Surface, admin_guard: L) -> Router
    where
        L: Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: Service<Request, Error = Infallible> + Clone + Send + Sync + 'static,
        <L::Service as Service<Request>>::Response: IntoResponse + 'static,
        <L::Service as Service<Request>>::Future: Send + 'static,
    {
        let fallback = self.fallback.map(Layered::finish);
        let unknown = fallback
            .clone()
            .unwrap_or_else(|| MethodRouter::new().fallback(|| async { StatusCode::NOT_FOUND }));
        let router = self
            .entries
            .into_iter()
            .fold(
                BTreeMap::<Cow<'static, str>, Vec<Entry<()>>>::new(),
                |mut paths, entry| {
                    paths.entry(entry.path.clone()).or_default().push(entry);
                    paths
                },
            )
            .into_iter()
            .fold(Router::new(), |router, (path, entries)| {
                let (served, hidden): (Vec<_>, Vec<_>) = entries
                    .into_iter()
                    .partition(|entry| entry.class.served_on(surface));
                let methods = served
                    .into_iter()
                    .map(|entry| guarded(entry, &admin_guard))
                    .reduce(MethodRouter::merge)
                    .unwrap_or_default();
                let methods = match hidden.is_empty() {
                    true => methods,
                    false => methods.fallback_service(unknown.clone()),
                };
                router.route(&path, methods)
            });
        match fallback {
            Some(fallback) => router.fallback_service(fallback),
            None => router,
        }
    }
}

fn unanswered(methods: &[Method]) -> MethodRouter {
    methods
        .iter()
        .filter_map(|method| MethodFilter::try_from(method.clone()).ok())
        .fold(MethodRouter::new(), |router, filter| {
            router.on(filter, || async { StatusCode::METHOD_NOT_ALLOWED })
        })
}

fn guarded<L>(entry: Entry<()>, admin_guard: &L) -> MethodRouter
where
    L: Layer<Route> + Clone + Send + Sync + 'static,
    L::Service: Service<Request, Error = Infallible> + Clone + Send + Sync + 'static,
    <L::Service as Service<Request>>::Response: IntoResponse + 'static,
    <L::Service as Service<Request>>::Future: Send + 'static,
{
    let Layered { router, wraps } = entry.served;
    Layered {
        router: match entry.class {
            RouteClass::Admin => router.route_layer(admin_guard.clone()),
            _ => router,
        },
        wraps,
    }
    .finish()
}

#[cfg(test)]
mod tests;
