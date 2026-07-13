use async_trait::async_trait;
use serde_json::{json, Value};
use std::{collections::BTreeSet, env, fs, path::PathBuf, sync::Arc, time::Duration};
use venice_provider_kernel::{
    canonical_digest, EncryptedSecret, ExecutionArtifact, ExecutionInput, ExecutionResult,
    Executor, FileStorage, Kernel, KernelConfig, SecretProtector, SubmissionReceipt,
};

const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 4, 0,
    0, 0, 181, 28, 12, 2, 0, 0, 0, 11, 73, 68, 65, 84, 120, 218, 99, 100, 248, 15, 0, 1, 5, 1, 1,
    39, 24, 227, 102, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

struct FakeUpstream {
    root: PathBuf,
}

#[async_trait]
impl Executor for FakeUpstream {
    async fn submit(
        &self,
        input: ExecutionInput,
        provider_request_id: &str,
    ) -> Result<SubmissionReceipt, String> {
        let path = self.root.join("fake-upstream-submissions.json");
        let mut submissions: Vec<Value> = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        submissions.push(json!({"providerOperationId":input.operation.provider_operation_id,"providerRequestId":provider_request_id,"capabilityId":input.operation.capability.id}));
        fs::write(&path, serde_json::to_vec_pretty(&submissions).unwrap())
            .map_err(|e| e.to_string())?;
        Ok(SubmissionReceipt {
            upstream_id: format!("fake-upstream-{provider_request_id}"),
            certainty: "submitted_confirmed".into(),
        })
    }

    async fn resume(
        &self,
        input: ExecutionInput,
        _upstream_id: &str,
    ) -> Result<ExecutionResult, String> {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        if input.operation.capability.id == "media.models.list"
            || input.operation.capability.id == "media.models.refresh"
        {
            let catalog = json!({"schemaVersion":"1.0","catalogRevision":"catalog-headless-v1","source":"live","models":[
                {"id":"background-remove","capabilityIds":["media.image.background-remove"],"controlsSchema":{"type":"object","properties":{},"additionalProperties":false}},
                {"id":"upscale","capabilityIds":["media.image.upscale"],"controlsSchema":{"type":"object","properties":{"scale":{"enum":[2,4]}},"required":["scale"],"additionalProperties":false}},
                {"id":"model-refresh","capabilityIds":["media.models.refresh"],"controlsSchema":{"type":"object","properties":{},"additionalProperties":false}},
                {"id":"model-list","capabilityIds":["media.models.list"],"controlsSchema":{"type":"object","properties":{},"additionalProperties":false}}
            ]});
            return Ok(ExecutionResult {
                artifacts: vec![],
                result: catalog.clone(),
                output: catalog,
            });
        }
        let sidecar = json!({
            "schema": "nekolegends.media-sidecar", "schemaVersion": 1,
            "app": "venice-provider-kernel-fake-upstream", "kind": "image",
            "createdAt": "2026-07-12T12:00:00.000Z"
        });
        let media_path = self
            .root
            .join(format!("{}.png", input.operation.provider_operation_id));
        fs::write(
            media_path.with_extension("sidecar.json"),
            serde_json::to_vec_pretty(&sidecar).unwrap(),
        )
        .map_err(|error| error.to_string())?;
        Ok(ExecutionResult {
            artifacts: vec![ExecutionArtifact {
                kind: "image".into(),
                mime_type: "image/png".into(),
                bytes: PNG.to_vec(),
                media: json!({"width":1,"height":1}),
                model: json!({"id":"background-remove"}),
                controls: input.operation.input.clone(),
                recipe: json!({"operation":"background-remove"}),
                source_evidence: None,
                source_path: Some(media_path),
            }],
            result: json!({}),
            output: json!({}),
        })
    }
    async fn finalize_artifact(
        &self,
        operation: &venice_provider_kernel::ProviderOperation,
        artifact_id: &str,
        sha256: &str,
        byte_size: u64,
        mut artifact: ExecutionArtifact,
    ) -> Result<ExecutionArtifact, String> {
        let path = artifact
            .source_path
            .as_ref()
            .unwrap()
            .with_extension("sidecar.json");
        let mut sidecar: Value =
            serde_json::from_slice(&fs::read(&path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        sidecar["providerArtifactId"] = json!(artifact_id);
        sidecar["providerOperationId"] = json!(operation.provider_operation_id);
        sidecar["sha256"] = json!(sha256);
        sidecar["byteSize"] = json!(byte_size);
        sidecar["controls"] = artifact.controls.clone();
        sidecar["recipe"] = artifact.recipe.clone();
        sidecar["sourceArtifacts"] = json!(operation.input_artifacts);
        fs::write(&path, serde_json::to_vec_pretty(&sidecar).unwrap())
            .map_err(|error| error.to_string())?;
        let verified: Value =
            serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
        artifact.source_evidence = Some(
            json!({"schemaIdentity":"nekolegends.media-sidecar","schemaVersion":1,"sanitizedSha256":canonical_digest(&verified).map_err(|error|error.to_string())?,"sanitizedSidecar":verified}),
        );
        Ok(artifact)
    }
}

struct DeterministicProtector;
impl SecretProtector for DeterministicProtector {
    fn protect(&self, plaintext: &[u8]) -> Result<EncryptedSecret, String> {
        let ciphertext = plaintext
            .iter()
            .map(|byte| format!("{:02x}", byte ^ 0x5a))
            .collect();
        Ok(EncryptedSecret {
            key_id: "deterministic-test-key".into(),
            ciphertext,
        })
    }
    fn unprotect(&self, encrypted: &EncryptedSecret) -> Result<Vec<u8>, String> {
        if encrypted.key_id != "deterministic-test-key" || encrypted.ciphertext.len() % 2 != 0 {
            return Err("invalid deterministic secret".into());
        }
        (0..encrypted.ciphertext.len())
            .step_by(2)
            .map(|index| {
                u8::from_str_radix(&encrypted.ciphertext[index..index + 2], 16)
                    .map(|byte| byte ^ 0x5a)
                    .map_err(|error| error.to_string())
            })
            .collect()
    }
}

#[tokio::main]
async fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if !(3..=4).contains(&args.len()) {
        eprintln!("usage: headless-test-provider ROOT CALLBACK_ORIGIN MANIFEST_DIGEST [PORT]");
        std::process::exit(2);
    }
    let root = PathBuf::from(&args[0]);
    let token = "headless-provider-test-token-00000000".to_string();
    let kernel = Kernel::open(KernelConfig {
        storage: Arc::new(FileStorage::new(root.clone())),
        token: token.clone(),
        manifest_digest: args[2].clone(),
        trusted_callback_origin: args[1].clone(),
        executor: Arc::new(FakeUpstream { root }),
        secret_protector: Arc::new(DeterministicProtector),
        callback_retry_base_ms: 100,
        terminal_replay_window_ms: 500,
        maintenance_interval_ms: 100,
        provider_id: "venice-media-local".into(),
        instance_id: "vml-headless-test".into(),
        shutdown_tx: None,
        token_scopes: BTreeSet::new(),
        admission: Default::default(),
        ownership_generation: 0,
        terminal_shutdown: Default::default(),
        shutdown_transaction: Default::default(),
    })
    .await
    .unwrap();
    let port = args.get(3).map(String::as_str).unwrap_or("0");
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    println!(
        "{}",
        json!({"baseUrl":format!("http://{address}"),"token":token})
    );
    axum::serve(listener, kernel.router()).await.unwrap();
}
