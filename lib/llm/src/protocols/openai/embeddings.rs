// SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use serde::{Deserialize, Serialize};
use validator::Validate;

mod nvext;

pub use nvext::{NvExt, NvExtProvider};

use dynamo_runtime::protocols::annotated::AnnotationsProvider;

#[derive(Serialize, Deserialize, Validate, Debug, Clone)]
pub struct NvCreateEmbeddingRequest {
    #[serde(flatten)]
    pub inner: async_openai::types::CreateEmbeddingRequest,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub nvext: Option<NvExt>,
}

/// A response structure for unary chat completion responses, embedding OpenAI's
/// `CreateChatCompletionResponse`.
///
/// # Fields
/// - `inner`: The base OpenAI unary chat completion response, embedded
///   using `serde(flatten)`.
#[derive(Serialize, Deserialize, Validate, Debug, Clone)]
pub struct NvCreateEmbeddingResponse {
    #[serde(flatten)]
    pub inner: async_openai::types::CreateEmbeddingResponse,
}

impl NvCreateEmbeddingResponse {
    pub fn empty() -> Self {
        Self {
            inner: async_openai::types::CreateEmbeddingResponse {
                object: "list".to_string(),
                model: "embedding".to_string(),
                data: vec![],
                usage: async_openai::types::EmbeddingUsage {
                    prompt_tokens: 0,
                    total_tokens: 0,
                },
            },
        }
    }
}

/// Implements `NvExtProvider` for `NvCr    eateEmbeddingRequest`,
/// providing access to NVIDIA-specific extensions.
impl NvExtProvider for NvCreateEmbeddingRequest {
    /// Returns a reference to the optional `NvExt` extension, if available.
    fn nvext(&self) -> Option<&NvExt> {
        self.nvext.as_ref()
    }
}

/// Implements `AnnotationsProvider` for `NvCreateEmbeddingRequest`,
/// enabling retrieval and management of request annotations.
impl AnnotationsProvider for NvCreateEmbeddingRequest {
    /// Retrieves the list of annotations from `NvExt`, if present.
    fn annotations(&self) -> Option<Vec<String>> {
        self.nvext
            .as_ref()
            .and_then(|nvext| nvext.annotations.clone())
    }

    /// Checks whether a specific annotation exists in the request.
    ///
    /// # Arguments
    /// * `annotation` - A string slice representing the annotation to check.
    ///
    /// # Returns
    /// `true` if the annotation exists, `false` otherwise.
    fn has_annotation(&self, annotation: &str) -> bool {
        self.nvext
            .as_ref()
            .and_then(|nvext| nvext.annotations.as_ref())
            .map(|annotations| annotations.contains(&annotation.to_string()))
            .unwrap_or(false)
    }
}
