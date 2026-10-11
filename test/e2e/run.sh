#!/usr/bin/env bash
# End-to-end test of kubedal on a kind cluster with an S3 compatible server.
#
# Covers persistent and inline volumes, cache and FUSE mounts, read-only and
# read-write access, and a file-list volume with upload.
#
# Needs: docker, kind, kubectl, helm.
# Environment:
#   CLUSTER       kind cluster name (default: kubedal-e2e)
#   IMAGE         kubedal image to build and load (default: kubedal:e2e)
#   S3_IMAGE      S3 server image (default: rustfs/rustfs:latest)
#   SKIP_BUILD=1  use an existing IMAGE instead of building it
#   KEEP_CLUSTER=1  keep the cluster after the run (also on failure)
set -euo pipefail

CLUSTER="${CLUSTER:-kubedal-e2e}"
IMAGE="${IMAGE:-kubedal:e2e}"
S3_IMAGE="${S3_IMAGE:-rustfs/rustfs:latest}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MANIFESTS="$ROOT/test/e2e/manifests"
NS=e2e
# Failures are written to a file, because pipelines run in subshells.
FAIL_FILE="$(mktemp)"

log() { printf '\n=== %s\n' "$*"; }
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  printf '%s\n' "$*" >>"$FAIL_FILE"
}

cleanup() {
  local status=$?
  if [[ $status -ne 0 || -s $FAIL_FILE ]]; then
    log "diagnostics"
    kubectl get pods -A -o wide || true
    kubectl -n "$NS" get events --sort-by=.lastTimestamp | tail -40 || true
    kubectl -n kube-system logs -l app.kubernetes.io/component=node -c kubedal --tail=200 || true
    kubectl -n kube-system logs -l app.kubernetes.io/component=controller -c kubedal --tail=100 || true
  fi
  if [[ "${KEEP_CLUSTER:-0}" != 1 ]]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

# Apply a pod from stdin, wait until it ends, print its logs, and check that it succeeded.
run_pod() {
  local name=$1 ns=${2:-$NS} phase=""
  kubectl apply -f - >/dev/null
  for _ in $(seq 1 120); do
    phase=$(kubectl -n "$ns" get pod "$name" -o jsonpath='{.status.phase}' 2>/dev/null || true)
    [[ $phase == Succeeded || $phase == Failed ]] && break
    sleep 2
  done
  kubectl -n "$ns" logs "$name" 2>/dev/null | sed "s/^/  [$name] /" || true
  if [[ $phase != Succeeded ]]; then
    kubectl -n "$ns" describe pod "$name" | tail -20
    fail "pod $name ended in phase '${phase:-unknown}'"
    return 1
  fi
}

# Delete a pod and wait, so kubelet unpublishes its volumes (and uploads results).
delete_pod() {
  kubectl -n "${2:-$NS}" delete pod "$1" --wait=true --timeout=180s >/dev/null
}

# A pod with one inline kubedal volume at /vol that runs a shell script.
inline_pod() {
  local name=$1 attrs=$2 script=$3 ns=${4:-$NS}
  cat <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $name
  namespace: $ns
spec:
  restartPolicy: Never
  terminationGracePeriodSeconds: 1
  containers:
    - name: test
      image: busybox:1.37
      command: ["sh", "-ec"]
      args:
        - |
$(printf '%s\n' "$script" | sed 's/^/          /')
      volumeMounts:
        - name: vol
          mountPath: /vol
  volumes:
    - name: vol
      csi:
        driver: kubedal.arunaengine.org
        volumeAttributes:
$(printf '%s\n' "$attrs" | sed 's/^/          /')
EOF
}

# Read a file from the DataNode with a fresh cache volume and compare it.
expect_object() {
  local path=$1 expected=$2 name="check-$RANDOM"
  inline_pod "$name" "dataNode: store
path: $(dirname "$path")
mount: cache-read-only" "test \"\$(cat /vol/$(basename "$path"))\" = '$expected'; echo found $path" \
    | run_pod "$name" || fail "object $path does not contain '$expected'"
  delete_pod "$name" || true
}

log "create kind cluster $CLUSTER"
if ! kind get clusters | grep -qx "$CLUSTER"; then
  kind create cluster --name "$CLUSTER" --wait 120s
fi
kubectl config use-context "kind-$CLUSTER" >/dev/null

if [[ "${SKIP_BUILD:-0}" != 1 ]]; then
  log "build image $IMAGE"
  docker build -t "$IMAGE" "$ROOT"
fi
kind load docker-image "$IMAGE" --name "$CLUSTER"

log "start S3 server and upload test data"
sed "s|S3_IMAGE|$S3_IMAGE|" "$MANIFESTS/s3.yaml" | kubectl apply -f -
kubectl -n "$NS" rollout status deployment/s3 --timeout=180s
kubectl -n "$NS" delete job seed --ignore-not-found >/dev/null 2>&1 || true
kubectl apply -f "$MANIFESTS/seed.yaml"
kubectl -n "$NS" wait --for=condition=complete job/seed --timeout=180s

log "install kubedal with Helm"
helm upgrade --install kubedal "$ROOT/charts/kubedal" --namespace kube-system \
  --set image.repository="${IMAGE%%:*}" --set image.tag="${IMAGE##*:}" \
  --set image.pullPolicy=Never --set logLevel='kubedal=debug\,info' --wait --timeout 180s
# Pick up a freshly loaded image with the same tag.
kubectl -n kube-system rollout restart daemonset/kubedal-node deployment/kubedal-controller
kubectl -n kube-system rollout status daemonset/kubedal-node --timeout=180s
kubectl -n kube-system rollout status deployment/kubedal-controller --timeout=180s
kubectl apply -f "$MANIFESTS/datanode.yaml"

log "inline cache read-only volume"
inline_pod cache-ro "dataNode: store
path: data
mount: cache-read-only" '
test "$(cat /vol/hello.txt)" = hello
test "$(cat /vol/sub/world.txt)" = world
if touch /vol/new 2>/dev/null; then echo "volume is writable"; exit 1; fi
echo ok' | run_pod cache-ro || true
delete_pod cache-ro || true

log "inline cache read-write volume with write-back"
inline_pod cache-rw "dataNode: store
path: results/cache-rw
mount: cache-read-write" '
mkdir -p /vol/sub
echo -n written > /vol/sub/out.txt
echo ok' | run_pod cache-rw || true
delete_pod cache-rw || true
expect_object results/cache-rw/sub/out.txt written

log "inline FUSE read-only volume"
inline_pod fuse-ro "dataNode: store
path: data
mount: fuse-read-only" '
test "$(cat /vol/hello.txt)" = hello
test "$(cat /vol/sub/world.txt)" = world
ls -la /vol
echo ok' | run_pod fuse-ro || true
delete_pod fuse-ro || true

log "inline FUSE read-write volume"
inline_pod fuse-rw "dataNode: store
path: results/fuse-rw
mount: fuse-read-write" '
echo -n via-fuse > /vol/fuse.txt
echo ok' | run_pod fuse-rw || true
delete_pod fuse-rw || true
expect_object results/fuse-rw/fuse.txt via-fuse

log "FUSE file system behaviour with a kernel mount"
inline_pod fuse-ops "dataNode: store
path: results/fuse-ops
mount: fuse-read-write" '
set -x
cd /vol
# Two handles append to the same file.
printf x > two.txt
exec 3>>two.txt 4>>two.txt
printf A >&3
printf B >&4
exec 3>&- 4>&-
test "$(cat two.txt)" = xAB
# Read what was written before the writer is closed.
exec 5>rw.txt
printf hello >&5
test "$(cat rw.txt)" = hello
exec 5>&-
# Truncate a file while a writer is open.
exec 6>t.txt
printf 12345 >&6
truncate -s 2 t.txt
exec 6>&-
test "$(cat t.txt)" = 12
# Remove a file while a writer is open; it must not come back.
exec 7>u.txt
printf data >&7
rm u.txt
printf more >&7
exec 7>&-
test ! -e u.txt
# Write a temp file, close it, then rename it to its final name.
printf final > result.tmp
mv result.tmp result.txt
test ! -e result.tmp
test "$(cat result.txt)" = final
# A descriptor keeps working after its file is renamed.
exec 7>moved.tmp
printf one >&7
mv moved.tmp moved.txt
printf " two" >&7
exec 7>&-
test "$(cat moved.txt)" = "one two"
# A reader that opens while a writer appends sees the new bytes.
printf x > ra.txt
exec 8>>ra.txt
exec 9<ra.txt
printf A >&8
test "$(cat <&9)" = xA
exec 8>&- 9<&-
# Append to a large file.
dd if=/dev/zero of=big.bin bs=1M count=20 2>/dev/null
printf tail >> big.bin
test "$(stat -c %s big.bin)" = 20971524
# An exclusive create works (mktemp opens with O_EXCL).
tmp=$(mktemp /vol/excl.XXXXXX)
printf excl > "$tmp"
test "$(cat "$tmp")" = excl
mv "$tmp" excl.txt
# With noclobber the shell creates a new file but never overwrites one.
set -C
printf new > clobber.txt
if printf again > clobber.txt; then exit 1; fi
set +C
test "$(cat clobber.txt)" = new
# A listing shows the live size of a file that is being written, so the
# kernel keeps the whole cached content.
printf abc > grow.txt
exec 3<>grow.txt
printf 123456789 >&3
ls -l /vol > /dev/null
test "$(wc -c < grow.txt)" -eq 9
test "$(cat grow.txt)" = 123456789
exec 3>&-
# An open file whose name was removed stays its own file next to a new file:
# the kernel must not get the same inode for both.
exec 4<>keep.txt
printf old-content >&4
exec 6<keep.txt
rm keep.txt
exec 7<>other.txt
printf new >&7
exec 8<other.txt
test "$(cat <&6)" = old-content
test "$(cat <&8)" = new
test "$(stat -L -c %i /proc/self/fd/6)" != "$(stat -L -c %i /proc/self/fd/8)"
exec 4>&- 6<&- 7>&- 8<&-
test ! -e keep.txt
echo ok' | run_pod fuse-ops || true
delete_pod fuse-ops || true
expect_object results/fuse-ops/two.txt xAB
expect_object results/fuse-ops/t.txt 12
expect_object results/fuse-ops/result.txt final
expect_object results/fuse-ops/ra.txt xA
expect_object results/fuse-ops/moved.txt "one two"
expect_object results/fuse-ops/excl.txt excl
expect_object results/fuse-ops/clobber.txt new
expect_object results/fuse-ops/grow.txt 123456789
expect_object results/fuse-ops/other.txt new
inline_pod fuse-ops-check "dataNode: store
path: results/fuse-ops
mount: cache-read-only" '
test ! -e /vol/u.txt
test ! -e /vol/result.tmp
test "$(stat -c %s /vol/big.bin)" = 20971524
test "$(tail -c 4 /vol/big.bin)" = tail
echo ok' | run_pod fuse-ops-check || fail "FUSE results in storage are wrong"
delete_pod fuse-ops-check || true

log "file-list volume with upload"
inline_pod file-list "dataNode: store
manifest: jobs/1.json" '
test "$(cat /vol/in/hello.txt)" = hello
test "$(cat /vol/in/deep/world.txt)" = world
test -x /vol/bin/run.sh
/vol/bin/run.sh
echo -n 42 > /vol/out/result.txt
echo ok' | run_pod file-list || true
delete_pod file-list || true
expect_object results/file-list/result.txt 42

log "persistent volume from a DataPod and PVC"
kubectl apply -f - <<EOF
apiVersion: kubedal.arunaengine.org/v1alpha1
kind: DataPod
metadata:
  name: data
  namespace: $NS
spec:
  path: /data
  dataNodeRef:
    name: store
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: data
  namespace: $NS
  annotations:
    kubedal.arunaengine.org/data-pod-name: data
    kubedal.arunaengine.org/mount: cache-read-only
spec:
  accessModes: ["ReadOnlyMany"]
  storageClassName: kubedal
  resources:
    requests:
      storage: 1Gi
EOF
run_pod persistent <<EOF || true
apiVersion: v1
kind: Pod
metadata:
  name: persistent
  namespace: $NS
spec:
  restartPolicy: Never
  terminationGracePeriodSeconds: 1
  containers:
    - name: test
      image: busybox:1.37
      command: ["sh", "-ec", "test \"\$(cat /vol/hello.txt)\" = hello; echo ok"]
      volumeMounts:
        - name: vol
          mountPath: /vol
  volumes:
    - name: vol
      persistentVolumeClaim:
        claimName: data
EOF
delete_pod persistent || true

log "a pod in another namespace may not use the DataNode"
kubectl create namespace e2e-other --dry-run=client -o yaml | kubectl apply -f - >/dev/null
inline_pod denied "dataNode: store
dataNodeNamespace: $NS
path: data" 'echo should not run' e2e-other | kubectl apply -f - >/dev/null
denied=""
for _ in $(seq 1 60); do
  if kubectl -n e2e-other get events --field-selector involvedObject.name=denied \
    -o jsonpath='{.items[*].message}' | grep -q "may not use DataNode"; then
    denied=yes
    break
  fi
  sleep 2
done
[[ $denied == yes ]] && echo "  access was denied as expected" || fail "pod in e2e-other was not denied"
kubectl -n e2e-other delete pod denied --wait=false >/dev/null || true

if [[ -s $FAIL_FILE ]]; then
  log "end-to-end test FAILED:"
  cat "$FAIL_FILE"
  exit 1
fi
log "end-to-end test passed"
