# KubeDAL: Universal Data Access Layer for Kubernetes

KubeDAL bridges the gap between Kubernetes workloads and any storage backend through a powerful CSI driver built with Rust and [Apache OpenDAL](https://github.com/apache/opendal).

> [!WARNING]  
> KubeDAL is currently in an early experimental development phase and nowhere near production ready, expect bugs, breaking changes and missing functionality.

## What is KubeDAL?

KubeDAL is a Kubernetes CSI driver that enables you to declare "Datasources" as first-class Kubernetes objects. These Data sources can point to virtually any storage backend supported by Apache OpenDAL, including AWS S3, Google Cloud Storage, Azure Blob Storage, HDFS, and many more.

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
# Define a storage Datasource
apiVersion: kubedal.arunaengine.org/v1alpha1
kind: Datasource
metadata:
  name: example-s3-dataset
spec:
  backend: S3 # Currently only S3 and HTTP are supported
  access_mode: ReadOnly # Can be ReadOnly or ReadWrite
  mount: Cached # Can be Cached or Fuse
  # You can specify any valid OpenDAL configuration entries either:
  # 1. Directly in the config field
  # 2. As a key/values in the referenced Secret (for sensitive information)
  credentials:
    secretRef:
      name: s3-credentials
      namespace: default
  config:
    bucket: my-bucket
    endpoint: http://localhost:9000
    region: us-east-1
    root: /foo/bar/my-root/
```

```yaml
# Reference in a PVC
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: quarterly-data
  annotations:
    kubedal.arunaengine.org/resource: example-s3-dataset # Resource must be in the same namespace
    kubedal.arunaengine.org/access-mode: cached # Optional, overwrite access mode
spec:
  storageClassName: kubedal
  accessModes:
    - ReadOnlyMany
  resources:
    requests:
      storage: 5Gi
```

## Join the Community

KubeDAL is open source and welcomes contributions! Join us in simplifying data access for Kubernetes workloads.

- GitHub: [github.com/arunaengine/kubedal](https://github.com/arunaengine/kubedal)
