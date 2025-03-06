use kube::{CustomResourceExt, api::ObjectMeta};
use kubedal::resource::crd::{Datasource, DatasourceSpec};

fn main() {
    // Generate the CRD yaml for our Resource type
    let crd = Datasource::crd();

    let path = std::env::args()
        .nth(1)
        .unwrap_or("./yamls/crd.yaml".to_string());

    let file = std::fs::File::create(path).expect("Failed to create file");

    // Print it to stdout
    serde_yaml::to_writer(file, &crd).expect("Failed to serialize CRD");

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
}
