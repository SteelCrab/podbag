//! The client against a fake engine on a Unix socket.

use super::fake::{FakeEngine, containers_json, json};
use super::*;
use std::sync::{Arc, Mutex};

type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

fn fake_engine(
    answer: impl Fn(&str, &str) -> fake::Answer + Send + Sync + 'static,
) -> (FakeEngine, Client, Seen) {
    let e = FakeEngine::start(answer);
    let (client, seen) = (e.client.clone(), e.seen.clone());
    (e, client, seen)
}

#[test]
fn lists_every_kind() {
    let (_dir, client, seen) = fake_engine(|_, path| match path {
        "/containers/json?all=1" => json(200, containers_json()),
        "/images/json" => json(
            200,
            serde_json::json!([{"Id": "sha256:0123456789abcdef", "RepoTags": null, "Size": 1000},
                               {"Id": "sha256:99", "RepoTags": ["alpine:3.20"], "Size": 7}]),
        ),
        "/volumes" => json(
            200,
            serde_json::json!({"Volumes": [{"Name": "data", "Driver": "local"}], "Warnings": null}),
        ),
        "/networks" => json(
            200,
            serde_json::json!([{"Name": "app", "Id": "n1", "Driver": "bridge",
                                "IPAM": {"Config": [{"Subnet": "10.77.1.0/24"}]}}]),
        ),
        _ => json(404, serde_json::json!({"message": "page not found"})),
    });
    let c = client.containers().unwrap();
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].name(), "web");
    assert!(c[0].running());
    assert_eq!(c[0].ports_text(), "8080->80/tcp");
    assert!(!c[1].running());
    assert_eq!(c[1].ports_text(), "");
    let i = client.images().unwrap();
    assert_eq!(i[0].name(), "0123456789ab");
    assert_eq!(i[1].name(), "alpine:3.20");
    assert_eq!(client.volumes().unwrap()[0].name, "data");
    let n = client.networks().unwrap();
    assert_eq!(n[0].subnet(), "10.77.1.0/24");
    assert!(!n[0].builtin());
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[test]
fn errors_carry_the_engine_message() {
    let (_dir, client, _) = fake_engine(|_, _| {
        json(
            409,
            serde_json::json!({"message": "container is running: stop it first"}),
        )
    });
    let e = client.remove(Kind::Containers, "web", false).unwrap_err();
    assert_eq!(e.status, 409);
    assert_eq!(e.message, "container is running: stop it first");
    assert!(!e.unsupported());
}

#[test]
fn actions_send_the_right_requests() {
    let (_dir, client, seen) = fake_engine(|method, path| match (method, path) {
        ("POST", "/containers/prune") => json(
            200,
            serde_json::json!({"ContainersDeleted": ["a", "b"], "SpaceReclaimed": 42}),
        ),
        ("POST", p) if p.starts_with("/images/prune") => json(
            200,
            serde_json::json!({"ImagesDeleted": null, "SpaceReclaimed": 0}),
        ),
        _ => (204, "text/plain", Vec::new()),
    });
    client.container_action("web", "stop").unwrap();
    client.remove(Kind::Containers, "web", true).unwrap();
    client.remove(Kind::Images, "alpine:3.20", false).unwrap();
    client.remove(Kind::Volumes, "data", true).unwrap();
    assert_eq!(
        client.prune(Kind::Containers).unwrap(),
        Pruned {
            count: 2,
            bytes: 42
        }
    );
    assert_eq!(client.prune(Kind::Images).unwrap(), Pruned::default());
    let seen = seen.lock().unwrap();
    let requests: Vec<(&str, &str)> = seen
        .iter()
        .map(|(m, p, _)| (m.as_str(), p.as_str()))
        .collect();
    assert_eq!(
        requests,
        [
            ("POST", "/containers/web/stop"),
            ("DELETE", "/containers/web?force=1"),
            ("DELETE", "/images/alpine%3A3.20"),
            ("DELETE", "/volumes/data"),
            ("POST", "/containers/prune"),
            (
                "POST",
                "/images/prune?filters=%7B%22dangling%22%3A%5B%22false%22%5D%7D"
            ),
        ]
    );
}

#[test]
fn pull_reports_progress_and_the_error_in_the_stream() {
    let (_dir, client, seen) = fake_engine(|_, path| {
        let body = if path.contains("missing") {
            "{\"status\":\"Pulling\"}\n{\"errorDetail\":{\"message\":\"manifest unknown\"},\"error\":\"manifest unknown\"}\n"
        } else {
            "{\"status\":\"Pulling from library/alpine\",\"id\":\"3.20\"}\n{\"status\":\"Downloading\",\"id\":\"abc\",\"progress\":\"[==>  ]\"}\n"
        };
        (200, "application/json", body.as_bytes().to_vec())
    });
    let mut lines = Vec::new();
    client
        .pull("alpine:3.20", |l| lines.push(l.to_string()))
        .unwrap();
    assert_eq!(
        lines,
        [
            "3.20: Pulling from library/alpine",
            "abc: Downloading [==>  ]"
        ]
    );
    let e = client.pull("missing", |_| {}).unwrap_err();
    assert_eq!(e.message, "manifest unknown");
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].1, "/images/create?fromImage=alpine&tag=3.20");
    assert_eq!(seen[1].1, "/images/create?fromImage=missing");
}

#[test]
fn logs_with_and_without_a_terminal() {
    let (_dir, client, _) = fake_engine(|_, path| {
        if path.starts_with("/containers/tty/") {
            (
                200,
                "application/vnd.docker.raw-stream",
                b"one\r\ntwo".to_vec(),
            )
        } else {
            let mut raw = Vec::new();
            for (s, t) in [(1u8, "out 1\nout "), (2, "2\nerr\n")] {
                raw.extend([s, 0, 0, 0]);
                raw.extend((t.len() as u32).to_be_bytes());
                raw.extend(t.as_bytes());
            }
            (200, "application/vnd.docker.multiplexed-stream", raw)
        }
    });
    let lines: Vec<String> = client
        .logs("tty", 100, false)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(lines, ["one", "two"]);
    let lines: Vec<String> = client
        .logs("plain", 100, true)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(lines, ["out 1", "out 2", "err"]);
}

#[test]
fn events_arrive_one_by_one() {
    let (_dir, client, _) = fake_engine(|_, _| {
        (
            200,
            "application/json",
            b"{\"Type\":\"container\",\"Action\":\"start\",\"Actor\":{\"ID\":\"abc\",\"Attributes\":{\"name\":\"web\"}},\"time\":1}\n{\"Type\":\"volume\",\"Action\":\"destroy\",\"Actor\":{\"ID\":\"data\"}}\n"
                .to_vec(),
        )
    });
    let events: Vec<Event> = client.events().unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, "container");
    assert_eq!(events[0].action, "start");
    assert_eq!(events[0].actor.id, "abc");
    assert_eq!(events[1].kind, "volume");
}

#[test]
fn features_podbag_lacks_are_found_missing() {
    // podbag today: 501 for stats, top, pause and rename.
    let (_dir, client, _) = fake_engine(|_, _| {
        json(
            501,
            serde_json::json!({"message": "stats is not supported by podbag"}),
        )
    });
    assert_eq!(client.features(Some("abc")), Features::default());
    // An engine that has them: the container that does not exist is "No such container".
    let (_dir2, client, _) = fake_engine(|_, path| {
        if path.contains("podbag-feature-check") {
            json(404, serde_json::json!({"message": "No such container: x"}))
        } else {
            json(200, serde_json::json!({}))
        }
    });
    assert_eq!(
        client.features(Some("abc")),
        Features {
            stats: true,
            top: true,
            pause: true,
            rename: true
        }
    );
    // An old engine without the route at all.
    let (_dir3, client, _) =
        fake_engine(|_, _| json(404, serde_json::json!({"message": "page not found"})));
    assert_eq!(client.features(None), Features::default());
}

#[test]
fn no_engine_is_status_zero() {
    let client = Client::new(Endpoint::Socket {
        path: "/nonexistent/podbag.sock".into(),
        own: false,
    });
    let e = client.ping().unwrap_err();
    assert_eq!(e.status, 0);
    assert!(e.message.contains("cannot connect"), "{}", e.message);
}

#[test]
fn tags_and_ids() {
    assert_eq!(split_tag("alpine"), ("alpine", None));
    assert_eq!(split_tag("alpine:3.20"), ("alpine", Some("3.20")));
    assert_eq!(
        split_tag("localhost:5000/app"),
        ("localhost:5000/app", None)
    );
    assert_eq!(
        split_tag("localhost:5000/app:1"),
        ("localhost:5000/app", Some("1"))
    );
    assert_eq!(split_tag("a@sha256:00"), ("a@sha256:00", None));
    assert_eq!(short_id("sha256:0123456789abcdef"), "0123456789ab");
    assert_eq!(short_id("abc"), "abc");
    assert_eq!(encode("a b/c:d"), "a%20b%2Fc%3Ad");
}
