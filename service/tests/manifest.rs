//! `--print-manifests` (design.md M11–M15, M30–M35): golden snapshots,
//! the authorization rule, the caller → callee service-map contract, name
//! and label validity over generated names, and refused options.
//!
//! The goldens under `tests/golden/` are compared byte for byte. Rewrite
//! them after an intended change with `MW_UPDATE_GOLDEN=1 cargo test -p
//! mw-service --test manifest`, then review the diff.
//! `scripts/check-manifests.sh` validates the same files with kubeconform.

use std::path::PathBuf;
use std::time::Duration;

use middleware::dns::unicast::map::srv_name;
use middleware::grpc::istio::is_dns_label;
use middleware::transport::{HandlerError, handler_fn};
use mw_service::config::parse_service_map;
use mw_service::manifest::{
    self, HEADLESS_SUFFIX, MAX_NAME_LEN, ManifestError, PullPolicy, Route, is_label_value,
};
use mw_service::{
    Command, ManifestOptions, Mebibytes, MilliCpu, Resources, Service, ServiceError, ServiceSpec,
};
use serde::Deserialize;
use serde_norway::Value;

fn echo_handler() -> impl middleware::Handler + Send + 'static {
    handler_fn(|req, out| {
        let dst = out
            .get_mut(..req.body.len())
            .ok_or(HandlerError::BufferTooSmall)?;
        dst.copy_from_slice(req.body);
        Ok(req.body.len())
    })
}

/// The demo services of the plan (§8) and a Zenoh-using gateway: one per
/// golden file.
fn fixtures() -> Vec<(&'static str, Service, ManifestOptions)> {
    let echo = Service::builder("demo/echo")
        .serve("demo/echo", echo_handler())
        .allow_callers([
            "spiffe://infra.local/ns/apps/sa/demo-caller",
            "spiffe://infra.local/robot/r1",
        ])
        .replicas(3)
        .build()
        .expect("demo/echo");
    let caller = Service::builder("demo/caller")
        .calls("demo/echo")
        .build()
        .expect("demo/caller");
    let gateway = Service::builder("mw/gateway")
        .serve("gateway/status", echo_handler())
        .calls("demo/echo")
        .calls("cam/jpeg")
        .allow_callers(["spiffe://infra.local/ns/ops/sa/mw"])
        .publishes("app/**")
        .subscribes("disc/**")
        .resources(Resources {
            cpu_request: MilliCpu(100),
            cpu_limit: MilliCpu(1000),
            memory_request: Mebibytes(128),
            memory_limit: Mebibytes(512),
        })
        .drain_grace(Duration::from_millis(30_500))
        .port(7000)
        .build()
        .expect("mw/gateway");
    let lab = |image: &str| ManifestOptions {
        image: Some(image.into()),
        pull_policy: PullPolicy::Never,
        ..ManifestOptions::new("apps")
    };
    let gateway_options = ManifestOptions {
        app_version: Some("0.6.0-rc1".into()),
        otlp_endpoint: Some("http://otel.observability.svc.cluster.local:4317".into()),
        zenoh_router: Some("tcp/zenohd.mesh.svc.cluster.local:7447".into()),
        routes: vec![Route::parse("cam/jpeg=cam.robots").expect("route")],
        ..lab("registry.local:5000/mw/gateway@sha256:0123")
    };
    vec![
        ("demo-echo", echo, lab("demo-echo:0.6.0")),
        ("demo-caller", caller, ManifestOptions::new("apps")),
        ("mw-gateway", gateway, gateway_options),
    ]
}

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.yaml"))
}

/// Renders through the command line path (`Service::execute`).
fn print_manifests(service: &Service, options: &ManifestOptions) -> String {
    let mut out = Vec::new();
    service
        .execute(&Command::PrintManifests(options.clone()), &mut out)
        .expect("renders");
    String::from_utf8(out).expect("utf-8")
}

fn documents(yaml: &str) -> Vec<Value> {
    serde_norway::Deserializer::from_str(yaml)
        .map(|d| Value::deserialize(d).expect("valid YAML"))
        .collect()
}

fn find<'v>(docs: &'v [Value], kind: &str, name: &str) -> &'v Value {
    docs.iter()
        .find(|d| d["kind"] == kind && d["metadata"]["name"] == name)
        .unwrap_or_else(|| panic!("no {kind} {name}"))
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v:?}"))
}

fn env_of<'v>(deployment: &'v Value, var: &str) -> Option<&'v Value> {
    deployment["spec"]["template"]["spec"]["containers"][0]["env"]
        .as_sequence()
        .expect("env")
        .iter()
        .find(|e| e["name"] == var)
}

#[test]
fn m13_goldens_match_byte_for_byte() {
    let update = std::env::var_os("MW_UPDATE_GOLDEN").is_some();
    for (name, service, options) in fixtures() {
        let rendered = print_manifests(&service, &options);
        assert_eq!(
            rendered,
            print_manifests(&service, &options),
            "{name}: deterministic"
        );
        let path = golden_path(name);
        if update {
            std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
            std::fs::write(&path, &rendered).expect("write golden");
            continue;
        }
        let golden = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (MW_UPDATE_GOLDEN=1 writes it)", path.display()));
        assert!(
            golden == rendered,
            "{name}: rendering differs from {} (MW_UPDATE_GOLDEN=1 rewrites it)\n{rendered}",
            path.display()
        );
    }
}

#[test]
fn m11_objects_and_deployment_contract() {
    let (_, echo, options) = fixtures().remove(0);
    let docs = documents(&print_manifests(&echo, &options));
    let kinds: Vec<(&str, &str)> = docs
        .iter()
        .map(|d| (str_of(&d["kind"]), str_of(&d["metadata"]["name"])))
        .collect();
    assert_eq!(
        kinds,
        [
            ("ServiceAccount", "demo-echo"),
            ("Service", "demo-echo"),
            ("Service", "demo-echo-pods"),
            ("Deployment", "demo-echo"),
            ("PodDisruptionBudget", "demo-echo"),
            ("AuthorizationPolicy", "demo-echo"),
            ("DestinationRule", "demo-echo"),
            ("DestinationRule", "demo-echo-pods"),
        ]
    );
    for d in &docs {
        assert_eq!(d["metadata"]["namespace"], "apps");
        let labels = &d["metadata"]["labels"];
        for (k, v) in [
            ("app.kubernetes.io/name", "demo-echo"),
            ("app.kubernetes.io/instance", "demo-echo"),
            ("app.kubernetes.io/version", "0.6.0"),
            ("app.kubernetes.io/managed-by", "mw-service"),
            ("app", "demo-echo"),
            ("version", "0.6.0"),
        ] {
            assert_eq!(labels[k], v, "{k} on {:?}", d["kind"]);
        }
    }
    let sa = find(&docs, "ServiceAccount", "demo-echo");
    assert_eq!(sa["automountServiceAccountToken"], false);
    let headless = find(&docs, "Service", "demo-echo-pods");
    assert_eq!(headless["spec"]["clusterIP"], "None");
    assert_eq!(headless["spec"]["publishNotReadyAddresses"], false);
    for svc in [find(&docs, "Service", "demo-echo"), headless] {
        let port = &svc["spec"]["ports"][0];
        assert_eq!(
            (&port["name"], &port["appProtocol"], &port["port"]),
            (&"grpc".into(), &"grpc".into(), &50051.into())
        );
    }
    let d = find(&docs, "Deployment", "demo-echo");
    assert_eq!(d["spec"]["replicas"], 3);
    let pod = &d["spec"]["template"];
    assert_eq!(pod["metadata"]["labels"]["sidecar.istio.io/inject"], "true");
    let annotations = pod["metadata"]["annotations"].as_mapping().expect("map");
    assert!(
        !annotations
            .keys()
            .any(|k| str_of(k).contains("spire") || str_of(k).contains("spiffe")),
        "the lab injects SPIRE by default: no annotation"
    );
    assert_eq!(
        pod["metadata"]["annotations"]["sidecar.istio.io/rewriteAppHTTPProbers"],
        "true"
    );
    let proxy: Value = serde_norway::from_str(str_of(
        &pod["metadata"]["annotations"]["proxy.istio.io/config"],
    ))
    .expect("ProxyConfig YAML");
    assert_eq!(proxy["holdApplicationUntilProxyStarts"], true);
    assert_eq!(proxy["terminationDrainDuration"], "7s");
    assert_eq!(
        proxy["proxyMetadata"]["EXIT_ON_ZERO_ACTIVE_CONNECTIONS"],
        "true"
    );
    assert_eq!(proxy["proxyMetadata"]["MINIMUM_DRAIN_DURATION"], "7s");
    let spec = &pod["spec"];
    assert_eq!(spec["serviceAccountName"], "demo-echo");
    assert_eq!(spec["terminationGracePeriodSeconds"], 15);
    assert_eq!(spec["securityContext"]["runAsNonRoot"], true);
    assert_eq!(
        spec["securityContext"]["seccompProfile"]["type"],
        "RuntimeDefault"
    );
    let c = &spec["containers"][0];
    assert_eq!(c["image"], "demo-echo:0.6.0");
    assert_eq!(c["imagePullPolicy"], "Never");
    assert_eq!(c["ports"][0]["name"], "grpc");
    assert_eq!(c["ports"][0]["containerPort"], 50051);
    for probe in ["startupProbe", "readinessProbe", "livenessProbe"] {
        assert_eq!(c[probe]["grpc"]["port"], 50051, "{probe}");
        assert_eq!(c[probe]["grpc"]["service"], "", "{probe}");
    }
    let sc = &c["securityContext"];
    assert_eq!(sc["readOnlyRootFilesystem"], true);
    assert_eq!(sc["allowPrivilegeEscalation"], false);
    assert_eq!(sc["runAsUser"], 65532);
    assert_eq!(sc["capabilities"]["drop"][0], "ALL");
    assert_eq!(c["resources"]["requests"]["cpu"], "50m");
    assert_eq!(c["resources"]["limits"]["memory"], "256Mi");
    assert_eq!(env_of(d, "MW_PORT").expect("MW_PORT")["value"], "50051");
    assert_eq!(
        env_of(d, "POD_NAMESPACE").expect("ns")["valueFrom"]["fieldRef"]["fieldPath"],
        "metadata.namespace"
    );
    assert_eq!(
        env_of(d, "POD_NAME").expect("pod")["valueFrom"]["fieldRef"]["fieldPath"],
        "metadata.name"
    );
    assert_eq!(
        env_of(d, "OTEL_EXPORTER_OTLP_ENDPOINT").expect("otlp")["value"],
        manifest::DEFAULT_OTLP_ENDPOINT
    );
    assert!(env_of(d, "MW_SERVICE_MAP").is_none(), "calls nothing");
    assert!(env_of(d, "MW_ZENOH_ROUTER").is_none(), "no pub/sub");
}

#[test]
fn m12_authorization_policy_destination_rules_and_pdb() {
    let mut f = fixtures();
    let (_, caller, caller_options) = f.remove(1);
    let (_, echo, echo_options) = f.remove(0);
    let docs = documents(&print_manifests(&echo, &echo_options));
    let ap = find(&docs, "AuthorizationPolicy", "demo-echo");
    assert_eq!(ap["apiVersion"], "security.istio.io/v1");
    assert_eq!(ap["spec"]["action"], "ALLOW");
    assert_eq!(
        ap["spec"]["selector"]["matchLabels"]["app.kubernetes.io/name"],
        "demo-echo"
    );
    let rules = ap["spec"]["rules"].as_sequence().expect("rules");
    assert_eq!(rules.len(), 1, "one rule, sources only: no port or path");
    let principals: Vec<&str> = rules[0]["from"][0]["source"]["principals"]
        .as_sequence()
        .expect("principals")
        .iter()
        .map(str_of)
        .collect();
    assert_eq!(
        principals,
        ["infra.local/ns/apps/sa/demo-caller", "infra.local/robot/r1"]
    );
    for (name, host) in [
        ("demo-echo", "demo-echo.apps.svc.cluster.local"),
        ("demo-echo-pods", "demo-echo-pods.apps.svc.cluster.local"),
    ] {
        let dr = find(&docs, "DestinationRule", name);
        assert_eq!(dr["apiVersion"], "networking.istio.io/v1");
        assert_eq!(dr["spec"]["host"], host);
        assert_eq!(dr["spec"]["trafficPolicy"]["tls"]["mode"], "ISTIO_MUTUAL");
    }
    let pdb = find(&docs, "PodDisruptionBudget", "demo-echo");
    assert_eq!(pdb["spec"]["minAvailable"], 1);

    // No callers: an ALLOW policy without rules (denies every caller; the
    // rewritten kubelet probes never reach Envoy's inbound listener), and
    // one replica gets no PDB.
    let docs = documents(&print_manifests(&caller, &caller_options));
    let ap = find(&docs, "AuthorizationPolicy", "demo-caller");
    assert_eq!(ap["spec"]["action"], "ALLOW");
    assert!(ap["spec"].get("rules").is_none(), "{ap:?}");
    assert!(!docs.iter().any(|d| d["kind"] == "PodDisruptionBudget"));
    let d = find(&docs, "Deployment", "demo-caller");
    assert_eq!(
        d["spec"]["template"]["metadata"]["annotations"]["sidecar.istio.io/rewriteAppHTTPProbers"],
        "true",
        "the probe rewrite the empty policy relies on"
    );
}

#[test]
fn m15_service_map_targets_the_callees_headless_service() {
    let mut f = fixtures();
    let (_, caller, caller_options) = f.remove(1);
    let (_, echo, echo_options) = f.remove(0);
    let caller_docs = documents(&print_manifests(&caller, &caller_options));
    let echo_docs = documents(&print_manifests(&echo, &echo_options));
    let d = find(&caller_docs, "Deployment", "demo-caller");
    let value = str_of(&env_of(d, "MW_SERVICE_MAP").expect("map")["value"]);
    assert_eq!(value, "demo/echo=demo-echo-pods.apps:grpc");
    let map = parse_service_map(value).expect("Config parses it");
    for kind in &caller.spec().calls {
        assert!(
            echo.spec().serves.contains(kind),
            "the callee serves {kind}"
        );
        let key = middleware::Key::new(kind).expect("kind");
        let target = map
            .get(middleware::discovery::ServiceKind::new(key))
            .expect("mapped");
        let headless = find(&echo_docs, "Service", target.service);
        assert_eq!(headless["spec"]["clusterIP"], "None");
        assert_eq!(headless["metadata"]["namespace"], target.namespace);
        assert_eq!(headless["spec"]["ports"][0]["name"], target.port_name);
        let mut buf = [0u8; 253];
        assert_eq!(
            srv_name(target, "cluster.local", &mut buf),
            Ok("_grpc._tcp.demo-echo-pods.apps.svc.cluster.local")
        );
    }
    // The callee admits the caller's identity (its ServiceAccount).
    let sa = str_of(&find(&caller_docs, "ServiceAccount", "demo-caller")["metadata"]["name"]);
    assert!(
        echo.spec()
            .allow_callers
            .contains(&format!("spiffe://infra.local/ns/apps/sa/{sa}"))
    );
}

#[test]
fn m31_zenoh_secret_mount_routes_and_overrides() {
    let (_, gw, options) = fixtures().remove(2);
    let docs = documents(&print_manifests(&gw, &options));
    let d = find(&docs, "Deployment", "mw-gateway");
    let value =
        |v: &str| str_of(&env_of(d, v).unwrap_or_else(|| panic!("{v}"))["value"]).to_owned();
    assert_eq!(
        value("MW_SERVICE_MAP"),
        "demo/echo=demo-echo-pods.apps:grpc,cam/jpeg=cam-pods.robots:grpc"
    );
    assert_eq!(
        value("MW_ZENOH_ROUTER"),
        "tcp/zenohd.mesh.svc.cluster.local:7447"
    );
    assert_eq!(
        value("MW_ZENOH_CREDENTIALS"),
        "/var/run/mw/zenoh/credentials"
    );
    assert_eq!(
        value("OTEL_EXPORTER_OTLP_ENDPOINT"),
        "http://otel.observability.svc.cluster.local:4317"
    );
    assert_eq!(value("MW_PORT"), "7000");
    let spec = &d["spec"]["template"]["spec"];
    let vol = spec["volumes"]
        .as_sequence()
        .expect("volumes")
        .iter()
        .find(|v| v["name"] == "zenoh-credentials")
        .expect("secret volume");
    assert_eq!(vol["secret"]["secretName"], "zenoh-mw-gateway");
    assert_eq!(vol["secret"]["items"][0]["key"], "credentials");
    assert_eq!(vol["secret"]["items"][0]["path"], "credentials");
    assert_eq!(vol["secret"]["defaultMode"], 0o440);
    let mount = spec["containers"][0]["volumeMounts"]
        .as_sequence()
        .expect("mounts")
        .iter()
        .find(|m| m["name"] == "zenoh-credentials")
        .expect("mount");
    assert_eq!(mount["mountPath"], "/var/run/mw/zenoh");
    assert_eq!(mount["readOnly"], true);
    // 30.5 s drain → 31 s + 10 grace, 31 s + 2 proxy drain.
    assert_eq!(spec["terminationGracePeriodSeconds"], 41);
    assert!(
        str_of(&d["spec"]["template"]["metadata"]["annotations"]["proxy.istio.io/config"])
            .contains("terminationDrainDuration: 33s")
    );
    // A digest has no tag: the version comes from --app-version.
    assert_eq!(d["metadata"]["labels"]["version"], "0.6.0-rc1");
    // Default zenoh router follows the namespace.
    let defaults = ManifestOptions {
        routes: options.routes.clone(),
        ..ManifestOptions::new("apps")
    };
    let docs = documents(&print_manifests(&gw, &defaults));
    let d = find(&docs, "Deployment", "mw-gateway");
    assert_eq!(
        env_of(d, "MW_ZENOH_ROUTER").expect("router")["value"],
        "tcp/zenohd.apps.svc.cluster.local:7447"
    );
    assert_eq!(
        d["metadata"]["labels"]["version"], "latest",
        "mw-gateway:latest"
    );
    assert_eq!(
        d["spec"]["template"]["spec"]["containers"][0]["imagePullPolicy"],
        "IfNotPresent"
    );
}

#[test]
fn m15_every_rendered_principal_is_a_converted_spiffe_id() {
    for (_, service, options) in fixtures() {
        let docs = documents(&print_manifests(&service, &options));
        let ap = docs
            .iter()
            .find(|d| d["kind"] == "AuthorizationPolicy")
            .expect("policy");
        let rendered: Vec<String> = ap["spec"]["rules"]
            .as_sequence()
            .map(|r| {
                r.iter()
                    .flat_map(|rule| rule["from"][0]["source"]["principals"].as_sequence())
                    .flatten()
                    .map(|p| str_of(p).to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let want: Vec<String> = service
            .spec()
            .allow_callers
            .iter()
            .map(|id| {
                assert!(middleware::grpc::istio::is_spiffe_id(id));
                id.strip_prefix("spiffe://").expect("prefix").to_owned()
            })
            .collect();
        assert_eq!(rendered, want);
    }
}

/// Deterministic pseudo-random generator (no proptest dependency).
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn segment(&mut self) -> String {
        const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-";
        let len = 1 + (self.next() % 24) as usize;
        (0..len)
            .map(|_| char::from(ALPHABET[(self.next() % ALPHABET.len() as u64) as usize]))
            .collect()
    }

    fn name(&mut self) -> String {
        let n = 1 + (self.next() % 3) as usize;
        (0..n).map(|_| self.segment()).collect::<Vec<_>>().join("/")
    }
}

fn walk_names_and_labels(v: &Value, check: &mut impl FnMut(&str, &Value)) {
    match v {
        Value::Mapping(m) => {
            for (k, child) in m {
                if let Some(k) = k.as_str() {
                    check(k, child);
                }
                walk_names_and_labels(child, check);
            }
        }
        Value::Sequence(s) => s.iter().for_each(|c| walk_names_and_labels(c, check)),
        _ => {}
    }
}

#[test]
fn m32_generated_names_render_only_valid_names_and_labels() {
    let mut rng = Lcg(0x5eed);
    let (mut rendered, mut refused) = (0, 0);
    for _ in 0..400 {
        let mut spec = ServiceSpec::new(rng.name());
        spec.calls
            .push(format!("{}/{}", rng.segment(), rng.segment()));
        spec.allow_callers
            .push(format!("spiffe://infra.local/ns/apps/sa/{}", rng.segment()));
        spec.replicas = 1 + (rng.next() % 4) as u16;
        let result = manifest::render(&spec, &ManifestOptions::new("apps"));
        let Ok(yaml) = result else {
            // Refused only for a reason the spec or the name length gives.
            let short = spec.k8s_name().is_some_and(|n| n.len() <= MAX_NAME_LEN);
            let callee_short =
                mw_service::spec::k8s_name(&spec.calls[0]).is_some_and(|n| n.len() <= MAX_NAME_LEN);
            assert!(
                spec.validate().is_err() || !short || !callee_short,
                "{spec:?}: {result:?}"
            );
            refused += 1;
            continue;
        };
        rendered += 1;
        for doc in documents(&yaml) {
            let name = str_of(&doc["metadata"]["name"]);
            assert!(is_dns_label(name), "{name:?}");
            walk_names_and_labels(&doc, &mut |key, value| {
                if key == "labels" || key == "matchLabels" || key == "selector" {
                    for (k, v) in value.as_mapping().into_iter().flatten() {
                        if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                            assert!(is_label_value(v), "{k}={v:?}");
                        }
                    }
                }
                if key == "serviceAccountName" || key == "secretName" {
                    assert!(is_dns_label(str_of(value)), "{key}={value:?}");
                }
            });
        }
    }
    assert!(rendered > 100 && refused > 10, "{rendered} / {refused}");
}

#[test]
fn m33_options_and_names_are_checked() {
    let spec = |name: &str| {
        let mut s = ServiceSpec::new(name);
        s.calls.push("demo/echo".into());
        s
    };
    let ok = ManifestOptions::new("apps");
    let err = |s: &ServiceSpec, o: &ManifestOptions| manifest::render(s, o).unwrap_err();
    let long = "a".repeat(MAX_NAME_LEN + 1);
    assert!(manifest::render(&spec(&"a".repeat(MAX_NAME_LEN)), &ok).is_ok());
    assert!(matches!(
        err(&spec(&long), &ok),
        ServiceError::Manifest(ManifestError::NameTooLong(n)) if n == long
    ));
    assert_eq!(HEADLESS_SUFFIX.len() + MAX_NAME_LEN, 63);
    assert!(matches!(
        err(&spec("x"), &ManifestOptions::new("Apps")),
        ServiceError::Manifest(ManifestError::InvalidNamespace(_))
    ));
    assert!(matches!(
        err(&ServiceSpec::new("Bad"), &ok),
        ServiceError::Spec(_)
    ));
    let with = |f: fn(&mut ManifestOptions)| {
        let mut o = ok.clone();
        f(&mut o);
        o
    };
    assert!(matches!(
        err(&spec("x"), &with(|o| o.image = Some("a b".into()))),
        ServiceError::Manifest(ManifestError::InvalidImage(_))
    ));
    assert!(matches!(
        err(&spec("x"), &with(|o| o.app_version = Some("-v".into()))),
        ServiceError::Manifest(ManifestError::InvalidVersion(_))
    ));
    assert!(matches!(
        err(
            &spec("x"),
            &with(|o| o.routes = vec![Route::parse("a/b=c").unwrap()])
        ),
        ServiceError::Manifest(ManifestError::RouteNotCalled(_))
    ));
    assert!(matches!(
        err(
            &spec("x"),
            &with(|o| o.routes = vec![
                Route::parse("demo/echo=c").unwrap(),
                Route::parse("demo/echo=d").unwrap()
            ])
        ),
        ServiceError::Manifest(ManifestError::DuplicateRoute(_))
    ));
    assert!(matches!(
        err(
            &spec("x"),
            &with(|o| o.routes = vec![Route::parse("demo/echo=C.ns").unwrap()])
        ),
        ServiceError::Manifest(ManifestError::InvalidRoute(_))
    ));
    // The rendered environment must parse as the runner's Config.
    assert!(matches!(
        err(
            &spec("x"),
            &with(|o| o.otlp_endpoint = Some("alloy:4317".into()))
        ),
        ServiceError::Config(_)
    ));
    assert!(matches!(
        err(
            &{
                let mut s = spec("x");
                s.publishes.push("app/**".into());
                s
            },
            &with(|o| o.zenoh_router = Some("zenohd:7447".into()))
        ),
        ServiceError::Config(_)
    ));
    let mut many = ServiceSpec::new("x");
    many.calls = (0..=mw_service::config::SERVICE_MAP_CAPACITY)
        .map(|i| format!("k/s{i}"))
        .collect();
    assert!(matches!(err(&many, &ok), ServiceError::Config(_)));
}
