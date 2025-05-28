// SPDX-FileCopyrightText: Copyright (c) 2024-2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dynamo_runtime::component::{Component, Endpoint};
use dynamo_runtime::traits::DistributedRuntimeProvider;

use crate::discovery::ModelEntry;
use crate::key_value_store::{EtcdStorage, KeyValueStore, KeyValueStoreManager};
use crate::model_card::{self, ModelDeploymentCard};
use crate::model_type::ModelType;

mod network_name;
pub use network_name::ModelNetworkName;

/// Prefix for Hugging Face model repository
const HF_SCHEME: &str = "hf://";

/// What we call a model if the user didn't provide a name. Usually this means the name
/// is invisible, for example in a text chat.
const DEFAULT_NAME: &str = "dynamo";

#[derive(Debug, Clone)]
pub struct LocalModel {
    full_path: PathBuf,
    card: ModelDeploymentCard,
}

impl Default for LocalModel {
    fn default() -> Self {
        LocalModel {
            full_path: PathBuf::new(),
            card: ModelDeploymentCard::with_name_only(DEFAULT_NAME),
        }
    }
}

impl LocalModel {
    pub fn with_name_only(name: &str) -> Self {
        LocalModel {
            card: ModelDeploymentCard::with_name_only(name),
            ..Default::default()
        }
    }

    pub fn card(&self) -> &ModelDeploymentCard {
        &self.card
    }

    pub fn path(&self) -> &Path {
        &self.full_path
    }

    pub fn display_name(&self) -> &str {
        &self.card.display_name
    }

    pub fn service_name(&self) -> &str {
        &self.card.service_name
    }

    /// Override max number of tokens in context. We usually only do this to limit kv cache allocation.
    pub fn set_context_length(&mut self, context_length: usize) {
        self.card.context_length = context_length;
    }

    pub fn set_kv_cache_block_size(&mut self, block_size: usize) {
        self.card.kv_cache_block_size = block_size;
    }

    /// Make an LLM ready for use:
    /// - Download it from Hugging Face (and NGC in future) if necessary
    /// - Resolve the path
    /// - Load it's ModelDeploymentCard card
    /// - Name it correctly
    ///
    /// The model name will depend on what "model_path" is:
    /// - A folder: The last part of the folder name: "/data/llms/Qwen2.5-3B-Instruct" -> "Qwen2.5-3B-Instruct"
    /// - A file: The GGUF filename: "/data/llms/Qwen2.5-3B-Instruct-Q6_K.gguf" -> "Qwen2.5-3B-Instruct-Q6_K.gguf"
    /// - An HF repo: The HF repo name: "Qwen/Qwen2.5-3B-Instruct" stays the same
    pub async fn prepare(
        model_path: &str,
        override_config: Option<&Path>,
        override_name: Option<String>,
    ) -> anyhow::Result<LocalModel> {
        // Name it

        // Check for hf:// prefix first, in case we really want an HF repo but it conflicts
        // with a relative path.
        let is_hf_repo =
            model_path.starts_with(HF_SCHEME) || !fs::exists(model_path).unwrap_or(false);
        let relative_path = model_path.trim_start_matches(HF_SCHEME);

        let full_path = if is_hf_repo {
            // HF download if necessary
            super::hub::from_hf(relative_path).await?
        } else {
            fs::canonicalize(relative_path)?
        };

        let model_name = override_name.unwrap_or_else(|| {
            if is_hf_repo {
                // HF repos use their full name ("org/name") not the folder name
                relative_path.to_string()
            } else {
                full_path
                    .iter()
                    .next_back()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| {
                        // Panic because we can't do anything without a model
                        panic!("Invalid model path, too short: '{}'", full_path.display())
                    })
            }
        });

        // Load the ModelDeploymentCard

        // --model-config takes precedence over --model-path
        let model_config_path = override_config.unwrap_or(&full_path);
        let mut card = ModelDeploymentCard::load(&model_config_path).await?;
        card.set_name(&model_name);

        Ok(LocalModel { full_path, card })
    }

    /// Attach this model the endpoint. This registers it on the network
    /// allowing ingress to discover it.
    pub async fn attach(
        &mut self,
        endpoint: &Endpoint,
        model_type: ModelType,
    ) -> anyhow::Result<()> {
        // A static component doesn't have an etcd_client because it doesn't need to register
        let Some(etcd_client) = endpoint.drt().etcd_client() else {
            anyhow::bail!("Cannot attach to static endpoint");
        };
        self.ensure_unique(endpoint.component(), self.display_name())
            .await?;

        // Store model config files in NATS object store
        let nats_client = endpoint.drt().nats_client();
        self.card.move_to_nats(nats_client.clone()).await?;

        // Publish the Model Deployment Card to etcd
        let kvstore: Box<dyn KeyValueStore> = Box::new(EtcdStorage::new(etcd_client.clone()));
        let card_store = Arc::new(KeyValueStoreManager::new(kvstore));
        let key = self.card.slug().to_string();
        card_store
            .publish(model_card::ROOT_PATH, None, &key, &mut self.card)
            .await?;

        // Publish our ModelEntry to etcd. This allows ingress to find the model card.
        // (Why don't we put the model card directly under this key?)
        let network_name = ModelNetworkName::from_local(endpoint, etcd_client.lease_id());
        tracing::debug!("Registering with etcd as {network_name}");
        let model_registration = ModelEntry {
            name: self.display_name().to_string(),
            endpoint: endpoint.id(),
            model_type,
        };
        etcd_client
            .kv_create(
                network_name.to_string(),
                serde_json::to_vec_pretty(&model_registration)?,
                None, // use primary lease
            )
            .await
    }

    /// Ensure that each component serves only one model.
    /// We can have multiple instances of the same model running using the same component name
    /// (they get load balanced, and are differentiated in etcd by their lease_id).
    /// We cannot have multiple models with the same component name.
    ///
    /// Returns an error if there is already a component by this name serving a different model.
    async fn ensure_unique(&self, component: &Component, model_name: &str) -> anyhow::Result<()> {
        let Some(etcd_client) = component.drt().etcd_client() else {
            // A static component is necessarily unique, it cannot register
            return Ok(());
        };
        for endpoint_info in component.list_instances().await? {
            let network_name: ModelNetworkName = (&endpoint_info).into();
            let entry = network_name.load_entry(&etcd_client).await?;
            if entry.name != model_name {
                anyhow::bail!("Duplicate component. Attempt to register model {model_name} at {component}, which is already used by {network_name} running model {}.", entry.name);
            }
        }
        Ok(())
    }
}
