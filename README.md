# KubeDAL: Universal Data Access Layer for Kubernetes

KubeDAL bridges the gap between Kubernetes workloads and any storage backend through a powerful CSI driver built with Rust and Apache OpenDAL.

## What is KubeDAL?

KubeDAL is a Kubernetes CSI driver that enables you to declare storage "Resources" as first-class Kubernetes objects. These Resources can point to virtually any storage backend supported by Apache OpenDAL, including AWS S3, Google Cloud Storage, Azure Blob Storage, local filesystems, HDFS, and many more.

## Why KubeDAL?

Managing data access across diverse storage systems in Kubernetes has traditionally been complex and fragmented. KubeDAL solves this by providing a unified interface to access data from anywhere, while maintaining Kubernetes-native workflows.

## Key Features

KubeDAL creates a seamless experience for both developers and operators:

- **Kubernetes-native Resources**: Define storage locations as proper Kubernetes Resources with full RBAC support
- **Universal Storage Support**: Leverage OpenDAL's extensive backend support for virtually any storage system
- **Flexible Access Modes**: Choose between local caching for performance or direct FUSE mounting for real-time access
- **Simple PVC Integration**: Connect to Resources via simple PVC annotations
- **Zero Application Changes**: Works transparently with existing applications

## How It Works

1. Define your storage locations as KubeDAL Resources
2. Create PVCs with annotations referencing these Resources
3. KubeDAL either downloads and caches the data or mounts it directly via FUSE
4. Your pods can access the data as standard volumes

## Getting Started

```yaml
# Define a storage Resource
apiVersion: kubedal.arunaengine.org/v1alpha1
kind: Resource
metadata:
  name: quarterly-reports
spec:
  backend: s3
  bucket: company-data
  path: /reports/quarterly
  accessMode: ReadOnlyMany
  credentials:
    secretRef:
      name: s3-credentials
```

```yaml
# Reference in a PVC
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: quarterly-data
  annotations:
    kubedal.arunaengine.org/resource: quarterly-reports
    kubedal.arunaengine.org/access-mode: cached  # or "direct-mount"
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
