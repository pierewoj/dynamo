# Fluid: Cloud-Native Data Orchestration and Acceleration

Fluid is an open-source, cloud-native data orchestration and acceleration platform for Kubernetes. It virtualizes and accelerates data access from various sources (object storage, distributed file systems, cloud storage), making it ideal for AI, machine learning, and big data workloads.

---

## Table of Contents

1. [Key Features](#key-features)
2. [Installation](#installation)
3. [Quick Start](#quick-start)
4. [Mounting Data Sources](#mounting-data-sources)
    - [WebUFS Example](#webufs-example)
    - [S3 Example](#s3-example)
5. [Using HuggingFace Models with Fluid](#using-huggingface-models-with-fluid)
6. [Usage with Dynamo](#usage-with-dynamo)
7. [Troubleshooting & FAQ](#troubleshooting--faq)
8. [Resources](#resources)

---

## Key Features

- **Data Caching and Acceleration:** Cache remote data close to compute workloads for faster access.
- **Unified Data Access:** Access data from S3, HDFS, NFS, and more through a single interface.
- **Kubernetes Native:** Integrates with Kubernetes using CRDs for data management.
- **Scalability:** Supports large-scale data and compute clusters.

---

## Installation

Fluid can be installed on any Kubernetes cluster using Helm.

**Prerequisites:**
- Kubernetes >= 1.18
- `kubectl` >= 1.18
- `Helm` >= 3.5

**Quick Install:**
```sh
kubectl create ns fluid-system
helm repo add fluid https://fluid-cloudnative.github.io/charts
helm repo update
helm install fluid fluid/fluid -n fluid-system
```
For advanced configuration, see the [Fluid Installation Guide](https://fluid-cloudnative.github.io/docs/get-started/installation).

---

## Quick Start

1. **Install Fluid (see above).**
2. **Create a Dataset and Runtime (see examples below).**
3. **Mount the resulting PVC in your workload.**

---

## Mounting Data Sources

### WebUFS Example

WebUFS allows mounting HTTP/HTTPS sources as filesystems.

```yaml
# Mount a public HTTP directory as a Fluid Dataset
apiVersion: data.fluid.io/v1alpha1
kind: Dataset
metadata:
  name: webufs-model
spec:
  mounts:
    - mountPoint: https://myhost.org/path_to_my_model  # Replace with your HTTP source
      name: webufs-model
---
apiVersion: data.fluid.io/v1alpha1
kind: AlluxioRuntime
metadata:
  name: webufs-model
spec:
  replicas: 2
  tieredstore:
    levels:
      - mediumtype: MEM
        path: /dev/shm
        quota: 2Gi
        high: "0.95"
        low: "0.7"
```
> After applying, Fluid creates a PersistentVolumeClaim (PVC) named `webufs-model` containing the files.

---

### S3 Example

Mount an S3 bucket as a Fluid Dataset.

```yaml
# Mount an S3 bucket as a Fluid Dataset
apiVersion: data.fluid.io/v1alpha1
kind: Dataset
metadata:
  name: s3-model
spec:
  mounts:
    - mountPoint: s3://<your-bucket>  # Replace with your bucket name
      options:
        alluxio.underfs.s3.endpoint: http://minio:9000  # S3 endpoint (e.g., MinIO)
        alluxio.underfs.s3.disable.dns.buckets: "true"
        aws.secretKey: "<your-secret>"
        aws.accessKeyId: "<your-access-key>"
---
apiVersion: data.fluid.io/v1alpha1
kind: AlluxioRuntime
metadata:
  name: s3-model
spec:
  replicas: 1
  tieredstore:
    levels:
      - mediumtype: MEM
        path: /dev/shm
        quota: 1Gi
        high: "0.95"
        low: "0.7"
---
apiVersion: data.fluid.io/v1alpha1
kind: DataLoad
metadata:
  name: s3-model-loader
spec:
  dataset:
    name: s3-model
    namespace: <your-namespace>  # Replace with your namespace
  loadMetadata: true
  target:
    - path: "/"
      replicas: 1
```
> The resulting PVC is named `s3-model`.

---

## Using HuggingFace Models with Fluid

**Limitations:**
- HuggingFace models are not exposed as simple filesystems or buckets.
- No native integration exists between Fluid and the HuggingFace Hub API.

**Workaround: Download and Upload to S3/MinIO**

1. Download the model using the HuggingFace CLI or SDK.
2. Upload the model files to a supported storage backend (S3, GCS, NFS).
3. Mount that backend using Fluid.

**Example Pod to Download and Upload:**
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: download-hf-to-minio
spec:
  restartPolicy: Never
  containers:
    - name: downloader
      image: python:3.10-slim
      command: ["sh", "-c"]
      args:
        - |
          set -eux
          pip install --no-cache-dir huggingface_hub awscli
          BUCKET_NAME=hf-models
          ENDPOINT_URL=http://minio:9000
          MODEL_NAME=deepseek-ai/DeepSeek-R1-Distill-Llama-8B
          LOCAL_DIR=/tmp/model
          if ! aws --endpoint-url $ENDPOINT_URL s3 ls "s3://$BUCKET_NAME" > /dev/null 2>&1; then
            aws --endpoint-url $ENDPOINT_URL s3 mb "s3://$BUCKET_NAME"
          fi
          huggingface-cli download $MODEL_NAME --local-dir $LOCAL_DIR --local-dir-use-symlinks False
          aws --endpoint-url $ENDPOINT_URL s3 cp $LOCAL_DIR s3://$BUCKET_NAME/$MODEL_NAME --recursive
      env:
        - name: AWS_ACCESS_KEY_ID
          value: "<your-access-key>"
        - name: AWS_SECRET_ACCESS_KEY
          value: "<your-secret>"
      volumeMounts:
        - name: tmp-volume
          mountPath: /tmp/model
  volumes:
    - name: tmp-volume
      emptyDir: {}
```
> You can then use `s3://hf-models/deepseek-ai/DeepSeek-R1-Distill-Llama-8B` as your Dataset mount.

---

## Usage with Dynamo

Mount the Fluid-generated PVC in your DynamoGraphDeployment:

```yaml
apiVersion: nvidia.com/v1alpha1
kind: DynamoGraphDeployment
metadata:
  name: model-caching
spec:
  dynamoGraph: "frontend:3x6rl5b3gcnf5skh"
  envs:
    - name: HF_HOME
      value: /model
    - name: DYN_DEPLOYMENT_CONFIG
      value: '{"Common": {"model": "/model", ...}}'
  services:
    VllmWorker:
      pvc:
        name: s3-model
        mountPoint: /model
    Processor:
      pvc:
        name: s3-model
        mountPoint: /model
```

---

## Troubleshooting & FAQ

- **PVC not created?** Check Fluid and AlluxioRuntime pod logs.
- **Model not found?** Ensure the model was uploaded to the correct bucket/path.
- **Permission errors?** Verify S3/MinIO credentials and bucket policies.

---

## Resources

- [Fluid Documentation](https://fluid-cloudnative.github.io/)
- [Alluxio Documentation](https://docs.alluxio.io/)
- [MinIO Documentation](https://min.io/docs/)
- [HuggingFace Hub](https://huggingface.co/docs/hub/index)
- [Dynamo Documentation](README.md)