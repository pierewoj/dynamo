<!--
SPDX-FileCopyrightText: Copyright (c) 2025 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
-->

# Deploying Inference Graphs to Kubernetes (`dynamo deploy`)

This guide explains the deployment options available for Dynamo inference graphs in Kubernetes environments.

## Deployment Options

Dynamo provides two distinct deployment options that each serve different use cases:
1. Dynamo Cloud Kubernetes Platform is preferred in cases that support it
2. Manual Deployment with Helm Charts is suited to users who need more control over their deployments


### Dynamo Cloud Kubernetes Platform [PREFERRED]

The Dynamo Cloud Platform (`deploy/cloud/`) provides a managed deployment experience:

- Contains the infrastructure components required for the Dynamo cloud platform
- Used when deploying with the `dynamo deploy` CLI commands
- Provides a managed deployment experience

For detailed instructions on using the Dynamo Cloud Platform, see:
- [Dynamo Cloud Platform Guide](dynamo_cloud.md): walks through installing and configuring the Dynamo cloud components on your Kubernetes cluster.
- [Operator Deployment Guide](operator_deployment.md)

### Manual Deployment with Helm Charts

Users who need more control over their deployments can use the manual deployment path (`deploy/helm/`):

- Used for manually deploying inference graphs to Kubernetes
- Contains Helm charts and configurations for deploying individual inference pipelines
- Provides full control over deployment parameters
- Requires manual management of infrastructure components
- Documentation:
  - [Using the Deployment Script](manual_helm_deployment.md#using-the-deployment-script): all-in-one script for manual deployment
  - [Helm Deployment Guide](manual_helm_deployment.md#helm-deployment-guide): detailed instructions for manual deployment

## Getting Started with Helm Deploy

1. **For Dynamo Cloud Platform**:
   - Follow the [Dynamo Cloud Platform Guide](dynamo_cloud.md)
   - Deploy a Hello World pipeline using the [Operator Deployment Guide](operator_deployment.md)
   - Deploy a Dynamo LLM pipeline to Kubernetes [Deploy LLM Guide](../../examples/llm_deployment.md#deploy-to-kubernetes)
   - Model caching with [Fluid](model_caching_with_fluid.md)

2. **For Manual Deployment**:
   - Follow the [Manual Helm Deployment Guide](manual_helm_deployment.md)

## Example Deployments

See the [Hello World example](../../examples/hello_world.md#deploying-to-and-running-the-example-in-kubernetes) for a complete walkthrough of deploying a simple inference graph.

See the [LLM example](../../examples/llm_deployment.md#deploy-to-kubernetes) for a complete walkthrough of deploying a production-ready LLM inference pipeline to Kubernetes.