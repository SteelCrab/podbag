//! Run a container from the form: create and start it, and pull the image first when the
//! engine does not have it. No Tauri here, so it is tested against fake engines.

use crate::images;
use podbag::client::{ApiError, Client, RunSpec, Target, split_tag};

/// The engine does not have the image: `404 No such image` (Docker, podbag) or `image
/// not known` (Podman).
fn missing_image(e: &ApiError) -> bool {
    let message = e.message.to_ascii_lowercase();
    e.status == 404 && (message.contains("no such image") || message.contains("image not known"))
}

/// The reference to pull: the image with `:latest` when it names no tag or digest. A pull
/// without a tag would get every tag.
pub fn pull_reference(image: &str) -> String {
    let image = image.trim();
    match split_tag(image) {
        (_, None) if !image.contains('@') => format!("{image}:latest"),
        _ => image.to_string(),
    }
}

/// Create and start the container; gives its id. A missing image is pulled first, with
/// the login for its registry when the engine is local (`auth`, as in the pull panel).
pub fn run(
    client: &Client,
    target: &Target,
    spec: &RunSpec,
    auth: impl Fn(&str) -> Option<String>,
    progress: impl FnMut(&str),
) -> Result<String, String> {
    spec.check()?;
    match client.run_container(spec) {
        Err(e) if missing_image(&e) => {
            images::pull(client, target, &pull_reference(&spec.image), auth, progress)?;
            client.run_container(spec).map_err(|e| e.message)
        }
        result => result.map_err(|e| e.message),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use podbag::client::fake::{FakeEngine, json};
    use podbag::client::{Choice, PublishedPort};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn local(engine: &FakeEngine) -> Target {
        Target {
            choice: Choice::Docker,
            endpoint: engine.client.endpoint.clone(),
        }
    }

    fn spec(image: &str) -> RunSpec {
        RunSpec {
            image: image.into(),
            name: "web".into(),
            ports: vec![PublishedPort {
                host: 8080,
                container: 80,
                protocol: "tcp".into(),
            }],
            ..RunSpec::default()
        }
    }

    #[test]
    fn pull_references_name_a_tag() {
        assert_eq!(pull_reference(" nginx "), "nginx:latest");
        assert_eq!(pull_reference("nginx:alpine"), "nginx:alpine");
        assert_eq!(
            pull_reference("localhost:5000/team/app"),
            "localhost:5000/team/app:latest"
        );
        assert_eq!(pull_reference("ghcr.io/me/app:1"), "ghcr.io/me/app:1");
        assert_eq!(
            pull_reference("alpine@sha256:0123abcd"),
            "alpine@sha256:0123abcd"
        );
    }

    #[test]
    fn an_image_the_engine_has_runs_at_once() {
        let engine = FakeEngine::start(|_, path| match path {
            "/containers/create?name=web" => json(201, serde_json::json!({"Id": "abc123"})),
            _ => (204, "text/plain", Vec::new()),
        });
        let id = run(
            &engine.client,
            &local(&engine),
            &spec("nginx:alpine"),
            |_| panic!("no pull, no login"),
            |_| panic!("no pull"),
        )
        .unwrap();
        assert_eq!(id, "abc123");
        assert_eq!(
            engine.requests(),
            [
                "POST /containers/create?name=web",
                "POST /containers/abc123/start"
            ]
        );
    }

    #[test]
    fn a_missing_image_is_pulled_with_the_login_then_run() {
        let pulled = Arc::new(AtomicBool::new(false));
        let have = pulled.clone();
        let engine = FakeEngine::start(move |_, path| match path {
            p if p.starts_with("/images/create") => {
                have.store(true, Ordering::SeqCst);
                (
                    200,
                    "application/json",
                    b"{\"status\":\"Pulling from me/app\",\"id\":\"1\"}\n".to_vec(),
                )
            }
            "/containers/create?name=web" if !have.load(Ordering::SeqCst) => json(
                404,
                serde_json::json!({"message": "No such image: ghcr.io/me/app:latest"}),
            ),
            "/containers/create?name=web" => json(201, serde_json::json!({"Id": "abc123"})),
            _ => (204, "text/plain", Vec::new()),
        });
        let mut lines = Vec::new();
        let id = run(
            &engine.client,
            &local(&engine),
            &spec("ghcr.io/me/app"),
            |reference| {
                assert_eq!(reference, "ghcr.io/me/app:latest");
                Some("e30".into())
            },
            |line| lines.push(line.to_string()),
        )
        .unwrap();
        assert_eq!(id, "abc123");
        assert!(pulled.load(Ordering::SeqCst));
        assert_eq!(lines, ["1: Pulling from me/app"]);
        assert_eq!(
            engine.requests(),
            [
                "POST /containers/create?name=web",
                "POST /images/create?fromImage=ghcr.io%2Fme%2Fapp&tag=latest",
                "POST /containers/create?name=web",
                "POST /containers/abc123/start"
            ]
        );
        assert_eq!(engine.header(1, "X-Registry-Auth").as_deref(), Some("e30"));
    }

    #[test]
    fn engine_errors_come_back_as_they_are() {
        let engine = FakeEngine::start(|_, path| match path {
            p if p.starts_with("/images/create") => (
                200,
                "application/json",
                b"{\"error\":\"pull access denied for nope\"}\n".to_vec(),
            ),
            "/containers/create?name=web" => json(
                404,
                serde_json::json!({"message": "No such image: nope:latest"}),
            ),
            _ => json(
                409,
                serde_json::json!({"message": "Conflict. The container name \"/web\" is already in use"}),
            ),
        });
        let target = local(&engine);
        let e = run(&engine.client, &target, &spec("nope"), |_| None, |_| {}).unwrap_err();
        assert_eq!(e, "pull access denied for nope");
        let e = run(
            &engine.client,
            &target,
            &RunSpec {
                name: "other".into(),
                ..spec("nginx")
            },
            |_| None,
            |_| {},
        )
        .unwrap_err();
        assert!(e.contains("already in use"), "{e}");
        let requests = engine.requests().len();
        let e = run(
            &engine.client,
            &target,
            &RunSpec {
                env: vec!["NOPE".into()],
                ..spec("nginx")
            },
            |_| None,
            |_| {},
        )
        .unwrap_err();
        assert!(e.contains("KEY=VALUE"), "{e}");
        assert_eq!(engine.requests().len(), requests, "nothing sent");
    }
}
