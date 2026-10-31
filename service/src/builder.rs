//! [`Service`] and its [`ServiceBuilder`]: what an application engineer
//! writes.
//!
//! The builder is immutable by value: every method takes the builder and
//! returns the next one, recording the declaration in a [`ServiceSpec`]
//! (and, for `serve*`, the handler). Nothing is checked until
//! [`ServiceBuilder::build`], which validates the whole spec at once
//! ([`ServiceSpec::validate`]) and so reports the first mistake in field
//! order rather than the first call that made one.

use middleware::Codec;
use middleware::transport::{HandlerError, RequestMeta};
use middleware::typed::TypedHandler;
use std::time::Duration;

use crate::ctx::{BoxedHandler, ServiceHandler};
use crate::spec::{Resources, ServiceSpec, SpecError, duration_ms};

/// One served kind and its handler.
#[derive(Debug)]
pub struct Served {
    /// The kind (`Service/Method` key) the handler answers.
    pub kind: String,
    /// The handler.
    pub handler: BoxedHandler,
}

/// Declares a service; see the module docs. Made by [`Service::builder`].
#[derive(Debug)]
#[must_use = "a builder does nothing until `build`"]
pub struct ServiceBuilder {
    spec: ServiceSpec,
    served: Vec<Served>,
}

impl ServiceBuilder {
    /// Answers requests on `kind` (a `Service/Method` key such as
    /// `demo/echo`) with `handler`: a [`ServiceHandler`], or any plain
    /// [`Handler`](middleware::Handler) (closures included, see
    /// [`handler_fn`](middleware::transport::handler_fn)).
    pub fn serve(self, kind: &str, handler: impl ServiceHandler) -> Self {
        let mut spec = self.spec;
        spec.serves.push(kind.to_owned());
        let mut served = self.served;
        served.push(Served {
            kind: kind.to_owned(),
            handler: BoxedHandler::new(handler),
        });
        ServiceBuilder { spec, served }
    }

    /// Answers requests on `kind` with a typed function: the request is
    /// decoded with `codec`, `f` gets it with the caller's
    /// [`RequestMeta`], and its response is encoded with `codec`
    /// ([`TypedHandler`]: undecodable requests are `InvalidRequest`,
    /// responses that do not fit are `BufferTooSmall`).
    pub fn serve_typed<Req, Resp, C, F>(self, kind: &str, codec: C, f: F) -> Self
    where
        Req: 'static,
        Resp: 'static,
        C: Codec<Req> + Codec<Resp> + Send + 'static,
        F: FnMut(&Req, &RequestMeta<'_>) -> Result<Resp, HandlerError> + Send + 'static,
    {
        self.serve(kind, TypedHandler::<Req, Resp, C, F>::new(codec, f))
    }

    /// Declares a dependency on `kind`: handlers reach it with
    /// [`Ctx::client`](crate::Ctx::client), and the deployment must map it
    /// in `MW_SERVICE_MAP`.
    pub fn calls(self, kind: &str) -> Self {
        self.with_spec(|s| s.calls.push(kind.to_owned()))
    }

    /// Admits these callers (SPIFFE IDs, e.g.
    /// `spiffe://infra.local/ns/apps/sa/demo-caller`); may be called again
    /// to add more. A service that admits nobody answers only health
    /// probes.
    pub fn allow_callers<I, S>(self, callers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_spec(|s| s.allow_callers.extend(callers.into_iter().map(Into::into)))
    }

    /// Declares publishing on keys matching `filter` (`a/b`, or `a/b/**`
    /// for everything below `a/b`); [`Ctx::publisher`](crate::Ctx::publisher)
    /// refuses other keys.
    pub fn publishes(self, filter: &str) -> Self {
        self.with_spec(|s| s.publishes.push(filter.to_owned()))
    }

    /// Declares a subscription to keys matching `filter` (same form as
    /// [`publishes`](Self::publishes)).
    pub fn subscribes(self, filter: &str) -> Self {
        self.with_spec(|s| s.subscribes.push(filter.to_owned()))
    }

    /// Runs `n` pods (default 1).
    pub fn replicas(self, n: u16) -> Self {
        self.with_spec(|s| s.replicas = n)
    }

    /// Container resources (default [`Resources::DEFAULT`]).
    pub fn resources(self, resources: Resources) -> Self {
        self.with_spec(|s| s.resources = resources)
    }

    /// Keeps answering for `grace` after reporting `NOT_SERVING` on
    /// termination (default [`DEFAULT_DRAIN_GRACE`](crate::spec::DEFAULT_DRAIN_GRACE)).
    pub fn drain_grace(self, grace: Duration) -> Self {
        self.with_spec(|s| s.drain_grace_ms = duration_ms(grace))
    }

    /// Listens on `port` (default [`DEFAULT_PORT`](crate::spec::DEFAULT_PORT)).
    pub fn port(self, port: u16) -> Self {
        self.with_spec(|s| s.port = port)
    }

    /// The spec declared so far (not validated).
    pub fn spec(&self) -> &ServiceSpec {
        &self.spec
    }

    /// Validates the declarations into a [`Service`].
    pub fn build(self) -> Result<Service, SpecError> {
        self.spec.validate()?;
        Ok(Service {
            spec: self.spec,
            served: self.served,
        })
    }

    fn with_spec(self, f: impl FnOnce(&mut ServiceSpec)) -> Self {
        let mut spec = self.spec;
        f(&mut spec);
        ServiceBuilder { spec, ..self }
    }
}

/// A declared, validated service: its [`ServiceSpec`] and its handlers.
/// Run it with [`Service::run`] or, from `main`, [`Service::main`].
#[derive(Debug)]
pub struct Service {
    spec: ServiceSpec,
    served: Vec<Served>,
}

impl Service {
    /// Starts declaring the service `name` (a key such as `demo/echo`; its
    /// Kubernetes name joins the segments with `-`).
    pub fn builder(name: &str) -> ServiceBuilder {
        ServiceBuilder {
            spec: ServiceSpec::new(name),
            served: Vec::new(),
        }
    }

    /// What the service declared.
    pub fn spec(&self) -> &ServiceSpec {
        &self.spec
    }

    /// The spec and the handlers, in `serves` order, for a runner.
    pub fn into_parts(self) -> (ServiceSpec, Vec<Served>) {
        (self.spec, self.served)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Field, Mebibytes, MilliCpu};
    use middleware::transport::handler_fn;

    fn echo() -> impl middleware::Handler + Send + 'static {
        handler_fn(|r, out| {
            let dst = out
                .get_mut(..r.body.len())
                .ok_or(HandlerError::BufferTooSmall)?;
            dst.copy_from_slice(r.body);
            Ok(r.body.len())
        })
    }

    #[test]
    fn m1_builder_records_every_declaration_in_order() {
        let resources = Resources {
            cpu_request: MilliCpu(100),
            cpu_limit: MilliCpu(1000),
            memory_request: Mebibytes(128),
            memory_limit: Mebibytes(512),
        };
        let svc = Service::builder("demo/echo")
            .serve("demo/echo", echo())
            .serve("demo/reverse", echo())
            .calls("cam/jpeg")
            .allow_callers(["spiffe://infra.local/ns/apps/sa/demo-caller"])
            .allow_callers(vec![String::from("spiffe://infra.local/robot/r1")])
            .publishes("app/demo/**")
            .subscribes("app/robot/pose")
            .replicas(3)
            .resources(resources)
            .drain_grace(Duration::from_millis(2500))
            .port(8080)
            .build()
            .expect("valid");
        let spec = svc.spec();
        assert_eq!(spec.name, "demo/echo");
        assert_eq!(spec.serves, ["demo/echo", "demo/reverse"]);
        assert_eq!(spec.calls, ["cam/jpeg"]);
        assert_eq!(
            spec.allow_callers,
            [
                "spiffe://infra.local/ns/apps/sa/demo-caller",
                "spiffe://infra.local/robot/r1"
            ]
        );
        assert_eq!(spec.publishes, ["app/demo/**"]);
        assert_eq!(spec.subscribes, ["app/robot/pose"]);
        assert_eq!(
            (
                spec.replicas,
                spec.resources,
                spec.drain_grace_ms,
                spec.port
            ),
            (3, resources, 2500, 8080)
        );
        let (spec, served) = svc.into_parts();
        let kinds: Vec<_> = served.iter().map(|s| s.kind.as_str()).collect();
        assert_eq!(kinds, spec.serves);
    }

    #[test]
    fn m1_build_validates_the_whole_spec() {
        let e = Service::builder("demo/echo")
            .serve("demo/echo", echo())
            .allow_callers(["not-a-spiffe-id"])
            .build()
            .unwrap_err();
        assert_eq!(
            e,
            SpecError::Invalid {
                field: Field::AllowCallers,
                value: "not-a-spiffe-id".into()
            }
        );
        let e = Service::builder("demo/echo")
            .serve("bad", echo())
            .replicas(0)
            .build()
            .unwrap_err();
        assert!(
            matches!(
                e,
                SpecError::Invalid {
                    field: Field::Serves,
                    ..
                }
            ),
            "{e}"
        );
        assert_eq!(
            Service::builder("Nope").build().unwrap_err(),
            SpecError::InvalidName("Nope".into())
        );
        // A client-only service serving nothing is valid.
        let caller = Service::builder("demo/caller").calls("demo/echo").build();
        assert!(caller.is_ok());
    }

    #[test]
    fn m1_serve_typed_wraps_typed_handler() {
        use middleware::codec::Raw;
        use middleware::transport::{Request, RequestMeta};
        let svc = Service::builder("demo/echo")
            .serve_typed("demo/echo", Raw, |req: &[u8; 2], meta: &RequestMeta<'_>| {
                Ok([req[1], u8::from(meta.trace.is_some())])
            })
            .build()
            .unwrap();
        let (_, mut served) = svc.into_parts();
        let h = &mut served[0].handler;
        let mut links = crate::ctx::Links::new();
        let mut ctx = crate::ctx::Ctx::new(RequestMeta::NONE, None, &mut links);
        let req = Request::new(middleware::Key::from_static("demo/echo"), b"ab");
        let mut out = [0u8; 4];
        let n = block_on(h.call(&mut ctx, &req, &mut out));
        assert_eq!(n, Ok(2));
        assert_eq!(&out[..2], [b'b', 0]);
        let bad = Request::new(middleware::Key::from_static("demo/echo"), b"abc");
        assert_eq!(
            block_on(h.call(&mut ctx, &bad, &mut out)),
            Err(HandlerError::InvalidRequest)
        );
    }

    /// Runs a future that is ready without I/O.
    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(f)
    }
}
