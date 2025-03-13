use std::io::Write;

use kube::{CustomResourceExt, api::ObjectMeta};
use kubedal::resource::crd::{DataSourceRef, Datasource, DatasourceSpec, Sync};

fn main() {
    // Generate the CRD yaml for our Resource type
    let datasource_crd = Datasource::crd();
    let sync_crd = Sync::crd();

    let path = std::env::args()
        .nth(1)
        .unwrap_or("./yamls/crd.yaml".to_string());

    let mut file = std::fs::File::create(path).expect("Failed to create file");

    serde_yaml::to_writer(&mut file, &datasource_crd).expect("Failed to serialize CRD");
    file.write_all(b"\n---\n")
        .expect("Failed to write CRD separator");
    serde_yaml::to_writer(&mut file, &sync_crd).expect("Failed to serialize CRD");

    let demo_resource = Datasource {
        metadata: ObjectMeta {
            name: Some("my-resource".to_string()),
            ..Default::default()
        },
        spec: DatasourceSpec {
            backend: kubedal::resource::crd::Backend::S3,
            access_mode: kubedal::resource::crd::AccessMode::ReadOnly,
            mount: kubedal::resource::crd::MountMode::Cached,
            credentials: Some(kubedal::resource::crd::Credentials {
                secret_ref: kubedal::resource::crd::SecretRef {
                    name: "my-secret".to_string(),
                    namespace: Some("my-namespace".to_string()),
                },
            }),
            config: {
                let mut map = std::collections::HashMap::new();
                map.insert("endpoint".to_string(), "http://localhost:9000".to_string());
                map.insert("bucket".to_string(), "my-bucket".to_string());
                map.insert("root".to_string(), "/foo/bar/my-root/".to_string());
                map.insert("region".to_string(), "us-east-1".to_string());
                map
            },
        },
        status: None,
    };

    serde_yaml::to_writer(std::io::stdout(), &demo_resource).expect("Failed to serialize Resource");

    let demo_sync = Sync {
        metadata: ObjectMeta {
            name: Some("my-sync".to_string()),
            ..Default::default()
        },
        spec: kubedal::resource::crd::SyncSpec {
            source: DataSourceRef { name: "from".to_string(), namespace: None },
            destination: DataSourceRef { name: "to".to_string(), namespace: None },
            clean_up: true,
        },
        status: None,
    };

    std::io::stdout().write_all(b"\n---\n").expect("Failed to write Resource separator");

    serde_yaml::to_writer(std::io::stdout(), &demo_sync).expect("Failed to serialize Resource");
}
