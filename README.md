# KubeDAL: Universal Data Access Layer for Kubernetes

KubeDAL bridges the gap between Kubernetes workloads and any storage backend through a powerful CSI driver built with Rust and [Apache OpenDAL](https://github.com/apache/opendal).

> [!WARNING]  
> KubeDAL is currently in an early experimental development phase and nowhere near production ready, expect bugs, breaking changes and missing functionality.

## What is KubeDAL?

KubeDAL is a Kubernetes CSI driver that enables you to declare "DataNodes" as first-class Kubernetes objects. These Data sources can point to virtually any storage backend supported by Apache OpenDAL, including AWS S3, Google Cloud Storage, Azure Blob Storage, HDFS, and many more.

## Why KubeDAL?

Managing data access across diverse storage systems in Kubernetes has traditionally been complex and fragmented. KubeDAL solves this by providing a unified interface to access data from anywhere, while maintaining Kubernetes-native workflows.

## Key Features

KubeDAL creates a seamless experience for both developers and operators:

- **Kubernetes-native Resources**: Define storage locations as proper Kubernetes Resources with full RBAC support
- **Universal Storage Support**: Leverage OpenDAL's extensive backend support for virtually any storage system
- **Flexible Access Modes**: Choose between local caching for performance or direct FUSE mounting for real-time access
- **Simple PVC Integration**: Connect to Datasources via simple PVC annotations
- **Zero Application Changes**: Works transparently with existing applications

## How It Works

1. Define your storage locations as KubeDAL Datasource
2. Create PVCs with annotations referencing these Resources
3. KubeDAL either downloads and caches the data or mounts it directly via FUSE
4. Your pods can access the data as standard volumes

## Getting Started

```yaml
# Define a storage DataNode
apiVersion: kubedal.arunaengine.org/v1alpha1
kind: DataNode
metadata:
  name: example-s3-node
  namespace: default
  labels:
    region: eu-central-1
spec:
  backend: S3 # Currently only S3 and HTTP are supported
  read_only: true
  limit: 10G # Can be used to set a quota on associated DataPods
  
  # You can specify any valid OpenDAL configuration entries either:
  # 1. Directly in the config field
  # 2. As a key/values in the referenced Secret (for sensitive information)
  secretRef:
    name: s3-credentials
    namespace: default
  config:
    bucket: my-bucket
    endpoint: http://localhost:9000
    region: eu-central-1
    root: /foo
```

```yaml
# Example DataPod for S3
apiVersion: kubedal.arunaengine.org/v1alpha1
kind: DataPod
metadata:
  name: example-data-pod
spec:
  path: /data
  dataNodeRef:
    name: example-s3-node
    namespace: default
  # Optionally a selector can be used to specify a range of DataNodes
  #dataNodeSelector:
  #  region: eu-central
```

```yaml
# Reference in a PVC
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: example-dataset-pvc
  namespace: default
  annotations:
    kubedal.arunaengine.org/data-pod-name: example-data-pod
    kubedal.arunaengine.org/data-pod-namespace: default
    kubedal.arunaengine.org/mount: fuse-read-only
spec:
  accessModes:
    - ReadOnlyMany
  storageClassName: kubedal
  resources:
    requests:
      storage: 5Gi
```

## Join the Community

KubeDAL is open source and welcomes contributions! Join us in simplifying data access for Kubernetes workloads.

- GitHub: [github.com/arunaengine/kubedal](https://github.com/arunaengine/kubedal)
