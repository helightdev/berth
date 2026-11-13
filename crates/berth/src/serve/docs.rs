//! The API documents of `berth serve`: OpenAPI for the requests, AsyncAPI for
//! the streams. Both include each host's own API under `/v1/hosts/{name}`.

use berth_api::streams::{self, Stream};
use serde_json::{Value, json};

/// The OpenAPI document of berthd, which `berth serve` passes through.
fn host_document() -> Option<Value> {
    Some(berthd::openapi())
}

const NAME: &str = "A saved host";

/// The OpenAPI document as JSON.
pub fn openapi() -> Value {
    let (_, mut api) = super::api::documented().split_for_parts();
    api.info.title = "berth serve".into();
    api.info.version = berth_api::VERSION.into();
    api.info.description = Some(
        "One API over all hosts, with the inbox and a key-value store. Everything under \
         /v1/hosts/{name}/ that is not named here is that host's own API, passed through unchanged; \
         only the `secrets` task is refused, which `hosts.secrets` stands in for. Over TCP every request \
         carries the server's token or an OAuth 2.0 access token as a bearer, and needs the role its operation names. \
         The routes of the setup key take that key as the bearer instead."
            .into(),
    );
    api.info.license = None;
    api.info.contact = None;
    let mut document = serde_json::to_value(api).expect("the document is JSON");
    // A stream at a path of its own is a GET that answers 101.
    for stream in streams::SERVE {
        if document["paths"].get(stream.path).is_none() {
            document["paths"][stream.path] = json!({"get": upgrade(stream, stream.id)});
        }
    }
    match host_document() {
        Some(host) => merge_host(&mut document, &host),
        None => {
            document["paths"]["/v1/hosts/{name}/{path}"] = json!({"get": {
                "tags": ["hosts"],
                "summary": "The host's own API, passed through unchanged (any method; see berthd's document).",
                "operationId": "host_passed_through",
                "parameters": [
                    {"name": "name", "in": "path", "required": true, "description": NAME, "schema": {"type": "string"}},
                    {"name": "path", "in": "path", "required": true, "description": "A path of the host's API below /v1/", "schema": {"type": "string"}},
                ],
                "responses": {"200": {"description": "The host's answer"}, "502": {"description": "The host could not be reached"}},
            }});
        }
    }
    document
}

fn upgrade(stream: &Stream, id: &str) -> Value {
    let parameters: Vec<Value> = stream
        .parameters
        .iter()
        .map(|(name, meaning)| json!({"name": name, "in": "query", "required": false, "description": meaning, "schema": {"type": "string"}}))
        .collect();
    json!({
        "tags": ["streams"],
        "summary": stream.summary,
        "operationId": format!("stream_{}", id.replace('.', "_")),
        "parameters": parameters,
        "responses": {"101": {"description": "A WebSocket; its frames are in the AsyncAPI document"}},
    })
}

/// Put a host's paths under `/v1/hosts/{name}` and its schemas beside ours.
fn merge_host(document: &mut Value, host: &Value) {
    let name = json!({"name": "name", "in": "path", "required": true, "description": NAME, "schema": {"type": "string"}});
    for (path, item) in host["paths"].as_object().into_iter().flatten() {
        let Some(rest) = path.strip_prefix("/v1") else {
            continue;
        };
        let mut item = item.clone();
        for (_, operation) in item.as_object_mut().into_iter().flatten() {
            if !operation.is_object() {
                continue;
            }
            if let Some(id) = operation["operationId"].as_str() {
                operation["operationId"] = json!(format!("host_{id}"));
            }
            let mut parameters = vec![name.clone()];
            parameters.extend(
                operation["parameters"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
            operation["parameters"] = json!(parameters);
            let tags: Vec<Value> = operation["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|tag| json!(format!("host: {}", tag.as_str().unwrap_or(""))))
                .collect();
            operation["tags"] = json!(tags);
        }
        let target = format!("/v1/hosts/{{name}}{rest}");
        // What berth serve answers itself is not the host's.
        if document["paths"].get(&target).is_none() {
            document["paths"][target] = item;
        }
    }
    for (name, schema) in host["components"]["schemas"]
        .as_object()
        .into_iter()
        .flatten()
    {
        if document["components"]["schemas"].get(name).is_none() {
            document["components"]["schemas"][name] = schema.clone();
        }
    }
}

/// The AsyncAPI document as JSON.
/// The API by operation. A host's operations are berthd's, with `host`.
pub fn operations() -> Value {
    let (_, own) = super::api::documented().split_for_parts();
    let own = serde_json::to_value(own).expect("the document is JSON");
    let mut document = berthd::http::operations_of(
        "berth serve",
        "The operations of berth serve. Every operation of a host is one here too: on the portal with `\"host\": \"NAME\"` in the frame, over HTTP under /v1/hosts/NAME. Only the `secrets` task is refused; `hosts.secrets` sends the server's own. Each operation names the least role a caller over TCP needs: view, operate or admin.",
        berth_api::ops::SERVE,
        &own,
        &|op| streams::SERVE.iter().find(|stream| stream.path == op.path),
    );
    document["hosts"] = json!(true);
    document
}

pub fn asyncapi() -> Value {
    let keep = |text: String| -> &'static str { Box::leak(text.into_boxed_str()) };
    let hosted: Vec<Stream> = streams::HOST
        .iter()
        .map(|stream| Stream {
            id: keep(format!("host.{}", stream.id)),
            path: keep(format!(
                "/v1/hosts/{{name}}{}",
                stream.path.trim_start_matches("/v1")
            )),
            summary: stream.summary,
            level: "host",
            parameters: stream.parameters,
            resumable: stream.resumable,
            server: stream.server,
            client: stream.client,
        })
        .collect();
    let all: Vec<&Stream> = streams::SERVE.iter().chain(&hosted).collect();
    streams::asyncapi(
        "berth serve",
        "The WebSockets of berth serve: its own, and each host's under /v1/hosts/{name}. \
         Text frames are JSON, one message each; binary frames carry only session and job bytes.",
        &all,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_documents_name_every_route_and_stream() {
        let document = openapi();
        let paths = [
            "/v1/hosts",
            "/v1/hosts/{name}/apply",
            "/v1/jobs/{id}/output",
            "/v1/inbox/{host}/{id}/done",
            "/v1/kv/{namespace}/{key}",
            // The host's own, under its name.
            "/v1/hosts/{name}/sessions/{id}/output",
            "/v1/hosts/{name}/tasks",
        ];
        for path in paths {
            assert!(document["paths"].get(path).is_some(), "{path}");
        }
        // Every request and stream of ours is an operation with a name, and the other way round.
        let own = super::super::api::documented().split_for_parts().1;
        let own = serde_json::to_value(own).unwrap();
        for (path, item) in own["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                assert!(
                    berth_api::ops::SERVE.iter().any(|op| op.path == path
                        && op.method.eq_ignore_ascii_case(method)
                        && op.kind == berth_api::ops::Kind::Call),
                    "{method} {path} is no operation"
                );
            }
        }
        for op in berth_api::ops::SERVE {
            let known = match op.kind {
                berth_api::ops::Kind::Call => own["paths"][op.path]
                    .get(op.method.to_lowercase())
                    .is_some(),
                berth_api::ops::Kind::Stream => {
                    streams::SERVE.iter().any(|stream| stream.path == op.path)
                }
            };
            assert!(known, "the operation {} has no route", op.name);
        }
        // Every stream is there as a GET that can answer 101.
        for stream in streams::SERVE {
            assert!(
                document["paths"][stream.path]["get"]["responses"]
                    .get("101")
                    .is_some(),
                "{}",
                stream.path
            );
        }
        assert!(
            document["paths"]["/v1/kv/{namespace}/{key}"]
                .get("put")
                .is_some()
        );
        let streams = asyncapi();
        assert_eq!(
            streams["channels"]["host.session"]["address"],
            "/v1/hosts/{name}/sessions/{id}"
        );
        assert_eq!(streams["channels"]["events"]["address"], "/v1/events");
    }

    #[test]
    fn a_hosts_paths_go_under_its_name() {
        let mut document = json!({"paths": {"/v1/hosts/{name}/apply": {"post": {}}}, "components": {"schemas": {"Done": {"ours": true}}}});
        let host = json!({
            "paths": {
                "/v1/sessions/{id}": {"get": {"operationId": "session", "tags": ["sessions"], "parameters": [{"name": "id", "in": "path"}]}},
                "/v1/apply": {"post": {"operationId": "theirs"}},
            },
            "components": {"schemas": {"Done": {"ours": false}, "Session": {}}},
        });
        merge_host(&mut document, &host);
        let get = &document["paths"]["/v1/hosts/{name}/sessions/{id}"]["get"];
        assert_eq!(get["operationId"], "host_session");
        assert_eq!(get["parameters"][0]["name"], "name");
        assert_eq!(get["parameters"][1]["name"], "id");
        assert_eq!(get["tags"][0], "host: sessions");
        assert!(
            document["paths"]["/v1/hosts/{name}/apply"]["post"]
                .get("operationId")
                .is_none()
        );
        assert_eq!(document["components"]["schemas"]["Done"]["ours"], true);
        assert!(document["components"]["schemas"].get("Session").is_some());
    }
}
