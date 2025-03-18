use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::{CustomResourceExt, api::ObjectMeta};
use kubedal::resource::crd::{
    DataNode, DataNodeSpec, DataPod, DataReplicaSet, DataReplicaSetSpecTemplate, MatchLabels, Ref,
};
use std::io::Write;

fn main() {
    // Generate the CRD yaml for our Resource type
    let data_node_crd = DataNode::crd();
    let data_pod_crd = DataPod::crd();
    let data_replica_set_crd = DataReplicaSet::crd();

    let path = std::env::args()
        .nth(1)
        .unwrap_or("./yamls/crd.yaml".to_string());

    let mut file = std::fs::File::create(path).expect("Failed to create file");

    serde_yaml::to_writer(&mut file, &data_node_crd).expect("Failed to serialize CRD");
    file.write_all(b"\n---\n")
        .expect("Failed to write CRD separator");
    serde_yaml::to_writer(&mut file, &data_pod_crd).expect("Failed to serialize CRD");
    file.write_all(b"\n---\n")
        .expect("Failed to write CRD separator");
    serde_yaml::to_writer(&mut file, &data_replica_set_crd).expect("Failed to serialize CRD");

    let demo_data_node = DataNode {
        metadata: ObjectMeta {
            name: Some("my-data-node".to_string()),
            ..Default::default()
        },
        spec: DataNodeSpec {
            backend: kubedal::resource::crd::Backend::S3,
            secret_ref: Some(Ref {
                name: "my-secret".to_string(),
                namespace: Some("my-secret-namespace".into()),
            }),
            read_only: false,
            config: {
                let mut map = std::collections::HashMap::new();
                map.insert("endpoint".to_string(), "http://localhost:9000".to_string());
                map.insert("bucket".to_string(), "my-bucket".to_string());
                map.insert("region".to_string(), "us-east-1".to_string());
                map
            },
            limit: Some(Quantity("1Gi".to_string())),
        },
        status: None,
    };

    let mut std_io = std::io::stdout();

    serde_yaml::to_writer(&mut std_io, &demo_data_node).expect("Failed to serialize Resource");

    std_io
        .write_all(b"\n---\n")
        .expect("Failed to write Resource separator");

    let demo_data_pod = DataPod {
        metadata: ObjectMeta {
            name: Some("my-data-pod".to_string()),
            ..Default::default()
        },
        spec: kubedal::resource::crd::DataPodSpec {
            data_node_ref: Some(Ref {
                name: "my-data-node".to_string(),
                namespace: Some("my-data-node-namespace".into()),
            }),
            ..Default::default()
        },
        status: None,
    };

    serde_yaml::to_writer(&mut std_io, &demo_data_pod).expect("Failed to serialize Resource");

    std_io
        .write_all(b"\n---\n")
        .expect("Failed to write Resource separator");

    let demo_data_replica_set = DataReplicaSet {
        metadata: ObjectMeta {
            name: Some("my-data-replica-set".to_string()),
            ..Default::default()
        },
        spec: kubedal::resource::crd::DataReplicaSetSpec {
            replicas: 3,
            source_pod: Ref {
                name: "my-data-pod".to_string(),
                namespace: Some("my-data-pod-namespace".into()),
            },
            selector: MatchLabels {
                labels: {
                    let mut map = std::collections::BTreeMap::new();
                    map.insert("app".to_string(), "my-data-pods".to_string());
                    map
                },
            },
            template: DataReplicaSetSpecTemplate {
                metadata: kubedal::resource::crd::LabelsMetadata {
                    labels: {
                        let mut map = std::collections::BTreeMap::new();
                        map.insert("app".to_string(), "my-data-pods".to_string());
                        map
                    },
                },
                spec: kubedal::resource::crd::DataPodSpec {
                    path: Some("/data".to_string()),
                    ..Default::default()
                },
            },
        },
        status: None,
    };

    serde_yaml::to_writer(&mut std_io, &demo_data_replica_set)
        .expect("Failed to serialize Resource");
}
