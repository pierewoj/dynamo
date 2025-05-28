// SPDX-FileCopyrightText: Copyright (c) 2024-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::{future::Future, pin::Pin};
use std::{io::Read, sync::Arc, time::Duration};

use anyhow::Context;
use dynamo_llm::{backend::ExecutionContext, engines::StreamingEngine, local_model::LocalModel};
use dynamo_runtime::{CancellationToken, DistributedRuntime};

mod flags;
pub use flags::Flags;
mod input;
mod opt;
pub use dynamo_llm::request_template::RequestTemplate;
pub use opt::{Input, Output};
mod subprocess;

const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(2);

/// Where we will attach the vllm/sglang subprocess. Invisible to users.
pub const INTERNAL_ENDPOINT: &str = "dyn://dynamo.internal.worker";

/// Default size of a KV cache block. Override with --kv-cache-block-size
const DEFAULT_KV_CACHE_BLOCK_SIZE: usize = 16;

pub enum EngineConfig {
    /// Remote networked engines
    Dynamic,

    /// A Full service engine does it's own tokenization and prompt formatting.
    StaticFull {
        engine: Arc<dyn StreamingEngine>,
        model: Box<LocalModel>,
    },

    /// A core engine expects to be wrapped with pre/post processors that handle tokenization.
    StaticCore {
        engine: ExecutionContext,
        model: Box<LocalModel>,
    },
}

pub async fn run(
    runtime: dynamo_runtime::Runtime,
    in_opt: Input,
    out_opt: Output,
    flags: Flags,
) -> anyhow::Result<()> {
    if matches!(&in_opt, Input::Endpoint(_)) && matches!(&out_opt, Output::Dynamic) {
        anyhow::bail!("Cannot use endpoint for both in and out");
    }

    let cancel_token = runtime.primary_token();
    let maybe_path = flags
        .model_path_pos
        .clone()
        .or(flags.model_path_flag.clone());

    let mut local_model: LocalModel = match out_opt {
        // If output is dynamic we are ingress and don't have a local model, but making an
        // empty one cleans up the code.
        Output::Dynamic => Default::default(),

        // All other output types have a local model
        _ => {
            match &maybe_path {
                Some(model_path) => {
                    LocalModel::prepare(
                        model_path.to_str().context("Invalid UTF-8 in model path")?,
                        flags.model_config.as_deref(),
                        flags.model_name.clone(),
                    )
                    .await?
                }
                None => {
                    // echo_full engine doesn't need a path
                    match &flags.model_name {
                        Some(name) => LocalModel::with_name_only(name),
                        None => Default::default(),
                    }
                }
            }
        }
    };

    // Only set if user provides. Usually loaded from tokenizer_config.json
    if let Some(context_length) = flags.context_length {
        local_model.set_context_length(context_length);
    }
    // Always set, there is no engine provided default
    local_model.set_kv_cache_block_size(
        flags
            .kv_cache_block_size
            .unwrap_or(DEFAULT_KV_CACHE_BLOCK_SIZE),
    );

    let mut extra: Option<Pin<Box<dyn Future<Output = ()> + Send>>> = None; // vllm and sglang sub-process

    let template = if let Some(path) = flags.request_template.as_ref() {
        let template = RequestTemplate::load(path)?;
        tracing::debug!("Using request template: {template:?}");
        Some(template)
    } else {
        None
    };

    // We may need it later
    let card = local_model.card().clone();

    // Create the engine matching `out`
    let engine_config = match out_opt {
        Output::Dynamic => {
            // Sanity check - TODO probably make a general sanity check at start of method
            if flags.context_length.is_some() {
                anyhow::bail!("'--content-length' flag should only be used on the worker node, not on the ingress");
            }
            if flags.kv_cache_block_size.is_some() {
                anyhow::bail!("'--kv-cache-block-size' flag should only be used on the worker node, not on the ingress");
            }
            EngineConfig::Dynamic
        }
        Output::EchoFull => EngineConfig::StaticFull {
            model: Box::new(local_model),
            engine: dynamo_llm::engines::make_engine_full(),
        },
        Output::EchoCore => {
            let card = local_model.card();
            if !card.has_tokenizer() {
                anyhow::bail!(
                    "out=echo_core need to find the tokenizer. Pass flag --model-path <path>"
                );
            };
            EngineConfig::StaticCore {
                engine: dynamo_llm::engines::make_engine_core(),
                model: Box::new(local_model),
            }
        }
        #[cfg(feature = "mistralrs")]
        Output::MistralRs => EngineConfig::StaticFull {
            engine: dynamo_engine_mistralrs::make_engine(&local_model).await?,
            model: Box::new(local_model),
        },
        Output::SgLang => {
            if !local_model.path().is_dir() {
                // TODO Does sglang support GGUF? Can we make it work?
                anyhow::bail!("`--model-path should point at a HuggingFace repo checkout");
            }

            // If `in=dyn` we want the sglang subprocess to listen on that endpoint.
            // If not, then the endpoint isn't exposed so we invent an internal one.
            let endpoint = match &in_opt {
                Input::Endpoint(path) => path.parse()?,
                _ => INTERNAL_ENDPOINT.parse()?,
            };

            let multi_node_conf = dynamo_llm::engines::MultiNodeConfig {
                num_nodes: flags.num_nodes,
                node_rank: flags.node_rank,
                leader_addr: flags.leader_addr.clone().unwrap_or_default(),
            };
            let (py_script, child) = match subprocess::start(
                subprocess::sglang::PY,
                &local_model,
                &endpoint,
                flags.clone(),
                if flags.num_nodes <= 1 {
                    None
                } else {
                    Some(multi_node_conf)
                },
            )
            .await
            {
                Ok(x) => x,
                Err(err) => {
                    anyhow::bail!("Failed starting sglang sub-process: {err}");
                }
            };
            let cancel_token = cancel_token.clone();

            // Sub-process cleanup
            extra = Some(Box::pin(async move {
                stopper(cancel_token, child, py_script).await;
            }));
            EngineConfig::Dynamic
        }
        Output::Vllm => {
            if flags.base_gpu_id != 0 {
                anyhow::bail!("vllm does not support base_gpu_id. Set environment variable CUDA_VISIBLE_DEVICES instead.");
            }

            // If `in=dyn` we want the vllm subprocess to listen on that endpoint.
            // If not, then the endpoint isn't exposed so we invent an internal one.
            let endpoint = match &in_opt {
                Input::Endpoint(path) => path.parse()?,
                _ => INTERNAL_ENDPOINT.parse()?,
            };

            let (py_script, child) = match subprocess::start(
                subprocess::vllm::PY,
                &local_model,
                &endpoint,
                flags.clone(),
                None, // multi-node config. vllm uses `ray`, see guide
            )
            .await
            {
                Ok(x) => x,
                Err(err) => {
                    anyhow::bail!("Failed starting vllm sub-process: {err}");
                }
            };
            let cancel_token = cancel_token.clone();

            // Sub-process cleanup
            extra = Some(Box::pin(async move {
                stopper(cancel_token, child, py_script).await;
            }));
            EngineConfig::Dynamic
        }
        Output::Trtllm => {
            if flags.base_gpu_id != 0 {
                anyhow::bail!("TRTLLM does not support base_gpu_id. Set environment variable CUDA_VISIBLE_DEVICES instead.");
            }

            // If `in=dyn` we want the trtllm subprocess to listen on that endpoint.
            // If not, then the endpoint isn't exposed so we invent an internal one.
            let endpoint = match &in_opt {
                Input::Endpoint(path) => path.parse()?,
                _ => INTERNAL_ENDPOINT.parse()?,
            };

            let (py_script, child) = match subprocess::start(
                subprocess::trtllm::PY,
                &local_model,
                &endpoint,
                flags.clone(),
                None, // multi-node config. trtlllm uses `mpi`, see guide
            )
            .await
            {
                Ok(x) => x,
                Err(err) => {
                    anyhow::bail!("Failed starting trtllm sub-process: {err}");
                }
            };
            let cancel_token = cancel_token.clone();

            // Sub-process cleanup
            extra = Some(Box::pin(async move {
                stopper(cancel_token, child, py_script).await;
            }));
            EngineConfig::Dynamic
        }

        #[cfg(feature = "llamacpp")]
        Output::LlamaCpp => {
            if !local_model.path().is_file() {
                anyhow::bail!("--model-path should refer to a GGUF file. llama_cpp does not support safetensors.");
            }
            let engine =
                dynamo_engine_llamacpp::make_engine(cancel_token.clone(), &local_model).await?;
            EngineConfig::StaticCore {
                engine,
                model: Box::new(local_model),
            }
        }
    };

    match in_opt {
        Input::Http => {
            crate::input::http::run(runtime.clone(), flags, engine_config, template).await?;
        }
        Input::Text => {
            crate::input::text::run(runtime.clone(), flags, None, engine_config, template).await?;
        }
        Input::Stdin => {
            let mut prompt = String::new();
            std::io::stdin().read_to_string(&mut prompt).unwrap();
            crate::input::text::run(
                runtime.clone(),
                flags,
                Some(prompt),
                engine_config,
                template,
            )
            .await?;
        }
        Input::Batch(path) => {
            crate::input::batch::run(runtime.clone(), flags, card, path, engine_config, template)
                .await?;
        }
        Input::Endpoint(path) => {
            let distributed_runtime = DistributedRuntime::from_settings(runtime.clone()).await?;
            crate::input::endpoint::run(distributed_runtime, path, engine_config).await?;
        }
    }

    // Allow engines to ask main thread to wait on an extra future.
    // We use this to stop the vllm and sglang sub-process
    if let Some(extra) = extra {
        extra.await;
    }

    Ok(())
}

/// Wait for cancel_token to be cancelled, then stop the child as gracefully as possible.
/// Keeps the TempPath alive until the child is stopped.
async fn stopper(
    cancel_token: CancellationToken,
    mut child: tokio::process::Child,
    py_script: tempfile::TempPath,
) {
    cancel_token.cancelled().await;

    // Ask subprocess to stop gracefully
    if let Some(pid) = child.id() {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }

    tokio::select! {
        exit = child.wait() => {
            tracing::trace!("vllm sub-process graceful exit");
            match exit {
                Ok(exit_status) if exit_status.success() => {}
                Ok(exit_status) => {
                    // This is nearly always 15 (SIGTERM)
                    tracing::trace!("vllm sub-process non-0 exit: {exit_status}");
                }
                Err(err) => {
                    tracing::warn!("vllm sub-process error getting exit status: {err}");
                }
            }
        }
        _ = tokio::time::sleep(CHILD_STOP_TIMEOUT) => {
            // It didn't stop in time, kill it
            child.kill().await.expect("Failed killing vllm subprocess");
            let _ = child.wait().await;
        }
    }
    // This temporary file contains the python script running the engine. It deletes on drop.
    // Keep it alive until the engine has stopped.
    drop(py_script);
}
