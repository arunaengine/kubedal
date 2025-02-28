use kube::CustomResourceExt;
use kubedal::resource::crd::Resource;

fn main() {
    // Generate the CRD yaml for our Resource type
    let crd = Resource::crd();

    let path = std::env::args()
        .nth(1)
        .unwrap_or("./yamls/crd.yaml".to_string());

    let file = std::fs::File::create(path).expect("Failed to create file");

    // Print it to stdout
    serde_yaml::to_writer(file, &crd).expect("Failed to serialize CRD");
}
