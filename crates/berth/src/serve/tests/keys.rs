//! The key ring over the API.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn the_key_ring_is_kept_on_the_server_and_only_public_halves_leave() {
    let served = served().await;
    let mut client = served.client().await;
    let client = &mut client;
    let private = |said: &Value| said.to_string().contains("PRIVATE KEY");

    let (status, listed) = send(client, "GET", "/v1/keys", Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed["keys"][0]["name"], "berth");
    assert!(
        listed["keys"][0]["public"]
            .as_str()
            .unwrap()
            .starts_with("ssh-ed25519 ")
    );
    assert!(!private(&listed));

    // Without a body a key is made; with one, that private key is taken in.
    let (status, made) = send(client, "PUT", "/v1/keys/deploy", Value::Null).await;
    assert_eq!(status, 200, "{made}");
    assert!(made["fingerprint"].as_str().unwrap().starts_with("SHA256:") && !private(&made));
    assert_eq!(
        send(client, "PUT", "/v1/keys/deploy", Value::Null).await.0,
        409
    );
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("key");
    let keygen = |phrase: &str| {
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(file.with_extension("pub"));
        assert!(
            std::process::Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-C", "made-up", "-N", phrase, "-f"])
                .arg(&file)
                .status()
                .unwrap()
                .success()
        );
        std::fs::read(&file).unwrap()
    };
    let bytes = keygen("");
    let headers = [("content-type", "application/octet-stream")];
    let answer = client
        .send("PUT", "/v1/keys/imported", &headers, bytes.into())
        .await
        .unwrap();
    let taken: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(answer.status, 200, "{taken}");
    let public = std::fs::read_to_string(file.with_extension("pub")).unwrap();
    assert_eq!(
        berth_core::keys::ring::blob(taken["public"].as_str().unwrap()),
        berth_core::keys::ring::blob(&public)
    );
    let answer = client
        .send(
            "PUT",
            "/v1/keys/locked",
            &headers,
            keygen("made-up-phrase").into(),
        )
        .await
        .unwrap();
    assert_eq!(answer.status, 400);
    assert!(String::from_utf8_lossy(&answer.body).contains("passphrase"));

    // A key a host logs in with stays, berth's always does, and an unknown one is refused.
    let profile = json!({"builder": {}, "access": {"address": "192.0.2.7", "auth": "key:deploy"}});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", profile)
            .await
            .0,
        200
    );
    let (status, said) = send(client, "DELETE", "/v1/keys/deploy", Value::Null).await;
    assert_eq!(status, 409, "{said}");
    assert_eq!(
        send(client, "DELETE", "/v1/keys/berth", Value::Null)
            .await
            .0,
        409
    );
    let unknown = json!({"builder": {}, "access": {"address": "192.0.2.7", "auth": "key:gone"}});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/attic/profile", unknown)
            .await
            .0,
        400
    );
    let (_, listed) = send(client, "GET", "/v1/keys", Value::Null).await;
    let deploy = listed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["name"] == "deploy")
        .unwrap();
    assert_eq!(deploy["hosts"], json!(["studio"]));
    assert!(!private(&listed));
    assert_eq!(
        send(client, "DELETE", "/v1/keys/imported", Value::Null)
            .await
            .0,
        200
    );

    // A key no host takes is rotated at once, as a job.
    let before = made["fingerprint"].clone();
    let profile = json!({"builder": {}, "access": {"address": "192.0.2.7"}});
    assert_eq!(
        send(client, "PUT", "/v1/hosts/studio/profile", profile)
            .await
            .0,
        200
    );
    let (status, acted) = send(client, "POST", "/v1/keys/deploy/rotate", Value::Null).await;
    assert_eq!(status, 200, "{acted}");
    let (output, code) = job_ended(&served.socket, acted["job"]["id"].as_str().unwrap()).await;
    assert_eq!(code, 0, "{output}");
    assert!(!output.contains("PRIVATE KEY"));
    let (_, listed) = send(client, "GET", "/v1/keys", Value::Null).await;
    let deploy = listed["keys"]
        .as_array()
        .unwrap()
        .iter()
        .find(|key| key["name"] == "deploy")
        .unwrap()
        .clone();
    assert_ne!(deploy["fingerprint"], before);
    assert!(deploy.get("pending").is_none());
    assert_eq!(
        send(client, "POST", "/v1/keys/gone/rotate", Value::Null)
            .await
            .0,
        404
    );
}
