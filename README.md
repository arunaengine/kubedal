# kubedal

kubedal is a Kubernetes CSI driver that puts data from object storage into pods.
It is written in Rust and uses [Apache OpenDAL](https://github.com/apache/opendal)
to talk to storage backends.

A pod declares a kubedal volume. Before the pod starts, kubedal copies the
requested files to the node, or mounts them through FUSE. When the pod ends,
kubedal can upload the results back to object storage. Nodes share nothing: all
data moves through object storage. Credentials stay in the cluster, and pods never
see them.

> [!WARNING]
> kubedal is young. The API (`v1alpha1`) and the manifest format may still change.

## Features

- Storage backends: S3 and S3 compatible servers (MinIO, RustFS, Ceph and others),
  Google Cloud Storage, Azure Blob Storage, and HTTP (read-only).
- Storage locations are Kubernetes objects (`DataNode`). Credentials live in a
  Secret next to the DataNode. Only the kubedal node plugin reads them.
- Volume types:
  - **Inline volumes**: declared directly in the pod spec, no PVC needed.
  - **Persistent volumes**: a PVC that refers to a `DataPod` (a folder in a DataNode).
  - **File-list volumes**: an inline volume that places the files listed in a JSON
    manifest, creates empty folders, and uploads a folder when the pod ends.
- Mount modes:
  - `cache-read-only` (default): files are copied to the node and bind mounted read-only.
  - `cache-read-write`: like above, but writable. New and changed files are uploaded
    when the pod ends.
  - `fuse-read-only` and `fuse-read-write`: files are served directly from storage
    through FUSE, without copying.
- A content cache on each node for file-list volumes: a file with a SHA-256 digest is
  downloaded once per node and DataNode, even if many pods use it.
- A size limit per DataNode (`limit`), checked before and during downloads.
- Volume state is stored on the node. A restart of the node plugin does not lose
  track of volumes or of pending uploads.

## How it works

The driver has two parts:

- The **controller** (a Deployment) creates persistent volumes for PVCs and runs the
  controllers for the kubedal custom resources.
- The **node plugin** (a DaemonSet) does the real work in `NodePublishVolume` and
  `NodeUnpublishVolume`: it downloads files, mounts them into the pod, and uploads
  results.

For cache volumes, the node plugin downloads all files below a prefix into a
directory on the node. Then it bind mounts that directory into the pod. On
unpublish it unmounts the directory and uploads new and changed files (for writable
volumes). It deletes the local copy only after the upload worked. If the upload
fails, kubedal keeps the data, records a `VolumeUploadFailed` event on the pod, and
returns an error. kubelet then retries the unpublish later.

## Install

kubedal is installed with Helm. Every release publishes the chart and the
container image to the GitHub container registry:

| What | Where |
| --- | --- |
| Helm chart | `oci://ghcr.io/arunaengine/charts/kubedal` |
| Image (amd64 and arm64) | `ghcr.io/arunaengine/kubedal:<version>` and `:latest` |

Install the latest release into `kube-system`:

```sh
helm install kubedal oci://ghcr.io/arunaengine/charts/kubedal --namespace kube-system
```

Pin a version with `--version 0.2.0` (the chart version and the image tag are
always the same). Upgrade with `helm upgrade kubedal oci://ghcr.io/arunaengine/charts/kubedal
--namespace kube-system`. Change values with `--set` or a values file:

```sh
helm show values oci://ghcr.io/arunaengine/charts/kubedal > values.yaml
# edit values.yaml
helm install kubedal oci://ghcr.io/arunaengine/charts/kubedal --namespace kube-system -f values.yaml
```

To install from a checkout instead, use `./charts/kubedal` as the chart.

Useful values (see `charts/kubedal/values.yaml` for all of them):

| Value | Default | Meaning |
| --- | --- | --- |
| `image.repository` | `ghcr.io/arunaengine/kubedal` | kubedal image |
| `image.tag` | chart `appVersion` | image tag |
| `logLevel` | `info` | passed as `RUST_LOG` |
| `kubeletDir` | `/var/lib/kubelet` | kubelet directory on the nodes |
| `node.dataDir` | `<kubeletDir>/plugins/<driverName>` | volume data and state on the nodes |
| `node.contentCacheSize` | `10Gi` | size limit of the content cache |
| `node.fuseSpoolSize` | `10Gi` | space limit for FUSE files that are written but not uploaded yet |
| `node.tolerations` | tolerate everything | where the node plugin runs |
| `controller.replicas` | `1` | more than 1 turns on leader election |
| `storageClass.create` | `true` | create the `kubedal` StorageClass |

The chart installs the CRDs from its `crds` folder. Helm does not upgrade CRDs, so
after an upgrade that changes them, apply them again:
`kubectl apply -f charts/kubedal/crds` from a checkout of that version. For plain
manifests, run `helm template kubedal oci://ghcr.io/arunaengine/charts/kubedal
--namespace kube-system`.

To build the image yourself (amd64 and arm64 work):

```sh
docker build -t kubedal:dev .
```

### Releases

A release happens when the version in `Cargo.toml` changes on `main`
(`.github/workflows/release.yml`). The chart `version` and `appVersion` in
`charts/kubedal/Chart.yaml` must match it. The workflow pushes the image and
the chart to ghcr.io, then creates the `v<version>` tag and a GitHub release
with the packaged chart attached.

## DataNode

A DataNode describes a storage location. `config` takes any
[OpenDAL option](https://opendal.apache.org/docs/rust/opendal/services/index.html)
of the backend. Settings can also come from a ConfigMap (`configMapRef`), so
several DataNodes can share them, and credentials from a Secret (`secretRef`).
If a key is set in more than one place, the Secret beats the ConfigMap, and
`config` beats both.

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: s3-credentials
  namespace: team-a
type: Opaque
stringData:
  access_key_id: "..."
  secret_access_key: "..."
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: s3-shared
  namespace: team-a
data:
  endpoint: https://s3.example.com
  region: us-east-1
---
apiVersion: kubedal.arunaengine.org/v1alpha1
kind: DataNode
metadata:
  name: store
  namespace: team-a
spec:
  backend: S3          # S3, GCS, Azblob or HTTP
  read_only: false     # true forbids writable volumes and uploads
  limit: 10Gi          # largest amount of data one volume may copy to a node
  secretRef:
    name: s3-credentials   # must be in the namespace of the DataNode
  configMapRef:
    name: s3-shared        # optional, must be in the namespace of the DataNode
  config:
    bucket: my-bucket
    root: /projects/a      # optional folder inside the bucket
```

### Who may use a DataNode

- A pod may use any DataNode in its own namespace, the same way it may mount any
  Secret in its own namespace.
- To use a DataNode in another namespace, the service account of the pod needs RBAC
  permission to `get` that DataNode (a Role and RoleBinding in the namespace of the
  DataNode). kubedal checks this with a SubjectAccessReview.
- Pods never need access to the Secret.
- The Secret and the ConfigMap of a DataNode must be in the same namespace as
  the DataNode.

## Inline volumes

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: task
  namespace: team-a
spec:
  restartPolicy: Never
  containers:
    - name: task
      image: alpine
      command: ["sh", "-c", "wc -l /input/* > /output/counts.txt"]
      volumeMounts:
        - name: input
          mountPath: /input
        - name: output
          mountPath: /output
  volumes:
    - name: input
      csi:
        driver: kubedal.arunaengine.org
        volumeAttributes:
          dataNode: store
          path: datasets/run-1
          mount: cache-read-only
    - name: output
      csi:
        driver: kubedal.arunaengine.org
        volumeAttributes:
          dataNode: store
          path: results/run-1
          mount: cache-read-write
```

Volume attributes:

| Attribute | Required | Meaning |
| --- | --- | --- |
| `dataNode` | yes | name of the DataNode |
| `dataNodeNamespace` | no | namespace of the DataNode, default: namespace of the pod |
| `path` | no | prefix inside the DataNode, default: the whole DataNode. A path to a single object gives a volume with just that file. |
| `mount` | no | `cache-read-only` (default), `cache-read-write`, `fuse-read-only` or `fuse-read-write` |
| `manifest` | no | path of a manifest object. This makes it a file-list volume, and `path` and `mount` are ignored. |

`readOnly: true` in the `csi` volume source always makes the volume read-only.

## Persistent volumes

A DataPod is a folder inside a DataNode. A PVC refers to it with annotations:

```yaml
apiVersion: kubedal.arunaengine.org/v1alpha1
kind: DataPod
metadata:
  name: reference-genome
  namespace: team-a
spec:
  path: /references/hg38
  dataNodeRef:
    name: store
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: reference-genome
  namespace: team-a
  annotations:
    kubedal.arunaengine.org/data-pod-name: reference-genome
    kubedal.arunaengine.org/mount: cache-read-only
spec:
  accessModes: ["ReadOnlyMany"]
  storageClassName: kubedal
  resources:
    requests:
      storage: 5Gi
```

Instead of a DataPod, a PVC can name a DataNode directly with
`kubedal.arunaengine.org/data-node-name` (and optionally
`kubedal.arunaengine.org/data-node-namespace`).

Each pod that uses the PVC gets its own copy of the data on its node. kubedal does
not share writable data between pods. If several pods write to the same prefix,
the last upload wins for each file.

## File-list volumes

A file-list volume places exactly the files listed in a manifest. This fits
workflow engines: each task gets its inputs and an empty output folder, and its
outputs are uploaded when the task ends.

```yaml
volumes:
  - name: work
    csi:
      driver: kubedal.arunaengine.org
      volumeAttributes:
        dataNode: store
        manifest: jobs/42/manifest.json
```

### Manifest format (version 1)

The manifest is a JSON object stored in the DataNode of the volume. The JSON schema
is in [`schema/manifest-v1.schema.json`](schema/manifest-v1.schema.json).

```json
{
  "version": 1,
  "files": [
    {
      "path": "inputs/reads.fq",
      "source": "datasets/sample-1/reads.fq",
      "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
      "size": 1048576
    },
    { "path": "bin/analyze.sh", "source": "tools/analyze.sh", "executable": true }
  ],
  "directories": ["outputs", "tmp"],
  "upload": {
    "dataNode": "results",
    "dataNodeNamespace": "team-a",
    "prefix": "runs/42",
    "path": "outputs"
  }
}
```

| Field | Meaning |
| --- | --- |
| `version` | must be `1` |
| `files[].path` | path of the file inside the volume |
| `files[].source` | path of the object inside the DataNode of the volume |
| `files[].sha256` | optional digest. The file is verified and shared through the content cache. |
| `files[].size` | optional size in bytes. It is checked against the DataNode `limit` before any download, and compared after the download. |
| `files[].executable` | optional. Mode 0755 instead of 0644. |
| `directories` | empty folders to create |
| `upload.dataNode` | DataNode to upload to. It must not be read-only. |
| `upload.dataNodeNamespace` | optional, default: namespace of the pod |
| `upload.prefix` | prefix inside the upload DataNode |
| `upload.path` | folder inside the volume to upload. Empty means the whole volume. |

Rules:

- Paths must be relative and must not contain `..`.
- A path cannot be a file and a folder at the same time.
- Unknown fields are rejected, so typos show up as errors.
- The upload target is checked when the volume is created, not only at the end.
- Everything below `upload.path` is uploaded, also unchanged inputs in that folder.
  Keep outputs in their own folder.

### Content cache

Files with a `sha256` go through a cache on each node, below `node.dataDir`. On a
miss, the file is downloaded, checked against the digest, made read-only, and stored
under its digest and its DataNode. A cache hit needs the same DataNode, so knowing
a digest is not enough to get content from another DataNode. Each volume then gets its own copy of the cached file. On file
systems with reflink support (XFS, Btrfs), this copy shares the data blocks and is
cheap. Elsewhere it is a normal copy. A pod that writes to its copy cannot change
the cache. The least recently used files are removed when the cache grows over
`node.contentCacheSize`.

## Limitations

- **FUSE mounts end when the node plugin restarts**, for example on a DaemonSet
  update. kubedal unmounts the stale mount on start and records a `FuseMountLost`
  event on the pod. The pod has to be restarted. Cache volumes are not affected.
- In FUSE read-write mode, a file that is open for writing lives in one spool file
  on the node, shared by all handles of that file. Reads, size, truncate, unlink
  and rename see the spool content. The whole file is uploaded on every `close`
  and `fsync`, and upload errors are returned there. If the upload keeps failing,
  the spool file stays on the node and is uploaded before the next mount or on
  unpublish (also after a restart of the driver), but only to the same DataNode
  object it was written for. Every upload sends the whole file, so use
  `cache-read-write` for programs that write a lot.
- Spool files on a node may use at most `node.fuseSpoolSize`. Writes past it fail
  with "no space left on device". Content that is not uploaded is never deleted.
- A reader keeps the file it opened: the shared spool if the file was being
  written, or else the object version it saw (on backends with ETags). If that
  object is replaced, further reads fail with ESTALE. On backends without ETags
  (for example HTTP), a reader can see a newer object under the same name.
- A FUSE folder listing keeps the names it saw when it started, so its later
  pages do not shift when files come and go. All open listings of a mount keep
  at most 1,000,000 names together. Past that, the oldest listing loses its
  names and makes a new list at its next call, so its pages may skip or repeat
  names.
- Renaming a closed file on S3, GCS or Azure Blob copies the object and removes the
  old one. Renaming a folder there fails with EXDEV, so tools like `mv` copy the
  files instead.
- `rename` with `RENAME_NOREPLACE` is supported (checked just before the rename,
  not atomic against another program creating the same name). `RENAME_EXCHANGE`
  is not supported.
- Write-back uploads new and changed files (compared by size, modification time,
  change time and inode). For pods with `fsGroup`, kubelet changes the owner of
  all files, which changes their change time, so all files are uploaded again.
  Files deleted in the pod are not deleted in storage. Symlinks and special files
  are not uploaded.
- Object storage does not keep file modes. Cache volumes use 0644 for files and 0755
  for folders. File-list volumes can mark files as executable.
- Empty folders survive a round trip only on backends that store folder objects. S3
  compatible backends do.
- The DataNode `limit` applies per volume. It is not checked for FUSE volumes.
- The HTTP backend cannot list, so it only works for paths to single files.
- `DataReplicaSet` is experimental. Its sync pod runs `kubedal replicate` from the
  kubedal image (the controller flags `--sync-image` and
  `--sync-image-pull-policy`, set by the chart from `image`). Every
  second it copies new and changed files from the source to the replicas. Files
  deleted in the source stay in the replicas. The sync pods use the chart
  `imagePullSecrets` too (controller flag `--sync-image-pull-secret`), and such
  a secret must exist in the namespace of the DataReplicaSet. Each file is
  copied to a temporary file `.kdc-<32 hex digits>` next to it first. If the
  sync pod is killed during a copy, that temporary file stays in the replica.

## Development

```sh
cargo build
cargo test                                   # unit and integration tests
cargo clippy --all-targets -- -D warnings
helm lint charts/kubedal
./test/e2e/run.sh                            # needs docker, kind, kubectl and helm
```

`tests/s3_minio.rs` runs against a real S3 compatible server when
`KUBEDAL_TEST_S3_ENDPOINT` is set. See the comment at the top of the file.
`tests/gcs_azblob.rs` does the same for Google Cloud Storage and Azure Blob
Storage emulators, when `KUBEDAL_TEST_GCS_ENDPOINT` or
`KUBEDAL_TEST_AZBLOB_ENDPOINT` is set.

kubedal builds fuse3 from the fork <https://github.com/arunaengine/fuse3> (set in
`[patch.crates-io]` in `Cargo.toml`). fuse3 0.9.0 gives an inode number to a new
file while an unlinked file with that number is still open, so the kernel mixes
the two files (attributes and cached pages). The fork keeps a number until the
kernel forgets the inode. The change is proposed upstream in
<https://github.com/Sherlock-Holo/fuse3/pull/139>; once it is
released there, the patch entry goes away.

After changing the CRD types, run `cargo run --bin crdgen` to regenerate
`charts/kubedal/crds/kubedal-crds.yaml`. After changing the manifest types, run
`UPDATE_SCHEMA=1 cargo test schema_file` to regenerate the JSON schema.

## License

Dual licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE). The FUSE file
system code in `src/util/fuse_fs` comes from the Apache-2.0 licensed `fuse3_opendal`
crate of the Apache OpenDAL project.
