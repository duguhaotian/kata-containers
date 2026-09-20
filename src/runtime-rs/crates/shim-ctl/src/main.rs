// Copyright (c) 2022 Red Hat
//
// SPDX-License-Identifier: Apache-2.0
//

use std::{env, path::PathBuf, time::Duration};

use anyhow::{bail, Context, Result};
use containerd_shim_protos::{sandbox_api::CheckpointSandboxRequest, sandbox_async::SandboxClient};
use ttrpc::{asynchronous::Client, context};

const WORKER_THREADS: usize = 2;

fn usage() -> &'static str {
    "usage: shim-ctl checkpoint <sandbox-id> <output-path> [namespace] [address]"
}

async fn checkpoint(args: &[String]) -> Result<()> {
    if !(4..=6).contains(&args.len()) {
        bail!(usage());
    }
    let sandbox_id = &args[2];
    let output_path = &args[3];
    let namespace = args.get(4).map(String::as_str).unwrap_or("k8s.io");
    let address = match args.get(5) {
        Some(address) => address.clone(),
        None => {
            let path = PathBuf::from("/run/containerd/io.containerd.runtime.v2.task")
                .join(namespace)
                .join(sandbox_id)
                .join("address");
            tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("read shim address {}", path.display()))?
        }
    };
    let address = address
        .trim()
        .strip_prefix("ttrpc+")
        .unwrap_or(address.trim());
    let client = Client::connect(address)
        .await
        .with_context(|| format!("connect to shim at {address}"))?;
    let client = SandboxClient::new(client);
    let request = CheckpointSandboxRequest {
        sandbox_id: sandbox_id.clone(),
        output_path: output_path.clone(),
        ..Default::default()
    };
    let ctx = context::with_timeout(Duration::from_secs(300).as_nanos() as i64);
    client
        .checkpoint_sandbox(ctx, &request)
        .await
        .context("CheckpointSandbox RPC")?;
    println!("checkpoint saved to {output_path}");
    Ok(())
}

async fn real_main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("checkpoint") => checkpoint(&args).await,
        _ => bail!(usage()),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
        .context("prepare tokio runtime")?;

    runtime.block_on(real_main()).map_err(Into::into)
}
